/* The C half of the Lamella <-> mbedTLS bridge: a handle-based, opaque-pointer API so the
 * Rust side never needs mbedTLS struct layouts (no bindgen). One lam_tls carries a whole
 * client session (contexts, RNG, CA chain); its ciphertext I/O goes through BIO callbacks
 * implemented in Rust (lamella_bio_send/recv over in-memory queues), so the library stays
 * a pure byte transform -- the managed SslStream does all socket work.
 *
 * Return conventions (mirrored by src/lib.rs):
 *   lam_tls_handshake:  1 established, 0 in progress (want read/write), <0 fatal.
 *   lam_tls_read:       >=0 plaintext bytes, LAM_TLS_WANT none yet, LAM_TLS_CLOSED
 *                       close-notify, LAM_TLS_ERR fatal.
 *   lam_tls_write:      >=0 bytes accepted, LAM_TLS_WANT try again, LAM_TLS_ERR fatal.
 */
#include <stddef.h>
#include <string.h>

#include <mbedtls/ctr_drbg.h>
#include <mbedtls/entropy.h>
#include <mbedtls/error.h>
#include <mbedtls/platform.h>
#include <mbedtls/ssl.h>
#include <mbedtls/x509_crt.h>

#define LAM_TLS_WANT (-1)
#define LAM_TLS_CLOSED (-2)
#define LAM_TLS_ERR (-3)

#if defined(LAMELLA_FREESTANDING_LIBC)
/* The string.h functions this build of mbedTLS needs (PEM delimiter search + name
 * scanning + comparisons): freestanding definitions for the libc-less firmware link,
 * compiled -fno-builtin so the compiler cannot fold a definition into a call to itself.
 * Host builds use the platform CRT. */
size_t strlen(const char *s)
{
    const char *end = s;
    while (*end != '\0') {
        end++;
    }
    return (size_t)(end - s);
}

int strcmp(const char *a, const char *b)
{
    while (*a != '\0' && *a == *b) {
        a++;
        b++;
    }
    return (unsigned char)*a - (unsigned char)*b;
}

int strncmp(const char *a, const char *b, size_t n)
{
    for (; n > 0; n--, a++, b++) {
        if (*a != *b || *a == '\0') {
            return (unsigned char)*a - (unsigned char)*b;
        }
    }
    return 0;
}

char *strchr(const char *s, int c)
{
    for (;; s++) {
        if (*s == (char)c) {
            return (char *)s;
        }
        if (*s == '\0') {
            return NULL;
        }
    }
}

char *strstr(const char *haystack, const char *needle)
{
    size_t needle_len = strlen(needle);
    if (needle_len == 0) {
        return (char *)haystack;
    }
    for (; *haystack != '\0'; haystack++) {
        if (*haystack == needle[0] && strncmp(haystack, needle, needle_len) == 0) {
            return (char *)haystack;
        }
    }
    return NULL;
}
#endif

/* Provided by src/lib.rs. `user` is the session's Rust-side BIO state. Both return byte
 * counts; recv returns 0 when no ciphertext is queued (mapped to WANT_READ here). */
extern int lamella_bio_send(void *user, const unsigned char *buf, size_t len);
extern int lamella_bio_recv(void *user, unsigned char *buf, size_t len);
/* Provided by src/lib.rs: fills `output` from the embedder's entropy source (e.g. the
 * SAM E54 TRNG on device); nonzero means the source failed. */
extern int lamella_entropy_poll(unsigned char *output, size_t len);

/* The MBEDTLS_ENTROPY_HARDWARE_ALT hook (declared in mbedtls/entropy.h's poll table). */
int mbedtls_hardware_poll(void *data, unsigned char *output, size_t len, size_t *olen)
{
    (void)data;
    if (lamella_entropy_poll(output, len) != 0) {
        return MBEDTLS_ERR_ENTROPY_SOURCE_FAILED;
    }
    *olen = len;
    return 0;
}

typedef struct lam_tls {
    mbedtls_ssl_context ssl;
    mbedtls_ssl_config conf;
    mbedtls_ctr_drbg_context drbg;
    mbedtls_entropy_context entropy;
    mbedtls_x509_crt ca;
    void *user;
    int have_ca;
} lam_tls;

static int lam_bio_send(void *ctx, const unsigned char *buf, size_t len)
{
    lam_tls *session = (lam_tls *)ctx;
    return lamella_bio_send(session->user, buf, len);
}

static int lam_bio_recv(void *ctx, unsigned char *buf, size_t len)
{
    lam_tls *session = (lam_tls *)ctx;
    int taken = lamella_bio_recv(session->user, buf, len);
    return taken == 0 ? MBEDTLS_ERR_SSL_WANT_READ : taken;
}

static void lam_tls_destroy(lam_tls *session)
{
    mbedtls_ssl_free(&session->ssl);
    mbedtls_ssl_config_free(&session->conf);
    mbedtls_ctr_drbg_free(&session->drbg);
    mbedtls_entropy_free(&session->entropy);
    mbedtls_x509_crt_free(&session->ca);
    mbedtls_free(session);
}

/* Builds a client session. verify_mode: 0 = required against ca_pem (a NUL-terminated PEM
 * bundle), 1 = accept-any (the managed validation callback owns the trust decision; the
 * handshake still proves key possession). `hostname` drives SNI + name verification.
 * Returns NULL on any setup failure. */
lam_tls *lam_tls_client_new(
    const unsigned char *ca_pem,
    size_t ca_len,
    const char *hostname,
    int verify_mode,
    void *user)
{
    lam_tls *session = (lam_tls *)mbedtls_calloc(1, sizeof(lam_tls));
    if (session == NULL) {
        return NULL;
    }
    mbedtls_ssl_init(&session->ssl);
    mbedtls_ssl_config_init(&session->conf);
    mbedtls_ctr_drbg_init(&session->drbg);
    mbedtls_entropy_init(&session->entropy);
    mbedtls_x509_crt_init(&session->ca);
    session->user = user;
    session->have_ca = 0;

    if (mbedtls_ctr_drbg_seed(
            &session->drbg, mbedtls_entropy_func, &session->entropy,
            (const unsigned char *)"lamella-tls", 11)
        != 0) {
        lam_tls_destroy(session);
        return NULL;
    }
    if (mbedtls_ssl_config_defaults(
            &session->conf, MBEDTLS_SSL_IS_CLIENT, MBEDTLS_SSL_TRANSPORT_STREAM,
            MBEDTLS_SSL_PRESET_DEFAULT)
        != 0) {
        lam_tls_destroy(session);
        return NULL;
    }
    mbedtls_ssl_conf_rng(&session->conf, mbedtls_ctr_drbg_random, &session->drbg);

    if (verify_mode == 0) {
        if (ca_pem == NULL || ca_len == 0
            || mbedtls_x509_crt_parse(&session->ca, ca_pem, ca_len) != 0) {
            lam_tls_destroy(session);
            return NULL;
        }
        session->have_ca = 1;
        mbedtls_ssl_conf_ca_chain(&session->conf, &session->ca, NULL);
        mbedtls_ssl_conf_authmode(&session->conf, MBEDTLS_SSL_VERIFY_REQUIRED);
    } else {
        mbedtls_ssl_conf_authmode(&session->conf, MBEDTLS_SSL_VERIFY_NONE);
    }

    if (mbedtls_ssl_setup(&session->ssl, &session->conf) != 0
        || mbedtls_ssl_set_hostname(&session->ssl, hostname) != 0) {
        lam_tls_destroy(session);
        return NULL;
    }
    mbedtls_ssl_set_bio(&session->ssl, session, lam_bio_send, lam_bio_recv, NULL);
    return session;
}

int lam_tls_handshake(lam_tls *session)
{
    int rc = mbedtls_ssl_handshake(&session->ssl);
    if (rc == 0) {
        return 1;
    }
    if (rc == MBEDTLS_ERR_SSL_WANT_READ || rc == MBEDTLS_ERR_SSL_WANT_WRITE) {
        return 0;
    }
    return LAM_TLS_ERR;
}

int lam_tls_read(lam_tls *session, unsigned char *buf, size_t len)
{
    int rc = mbedtls_ssl_read(&session->ssl, buf, len);
    if (rc >= 0) {
        /* A zero read from mbedTLS means the peer ended the connection. */
        return rc == 0 ? LAM_TLS_CLOSED : rc;
    }
    if (rc == MBEDTLS_ERR_SSL_WANT_READ || rc == MBEDTLS_ERR_SSL_WANT_WRITE) {
        return LAM_TLS_WANT;
    }
    if (rc == MBEDTLS_ERR_SSL_PEER_CLOSE_NOTIFY) {
        return LAM_TLS_CLOSED;
    }
    return LAM_TLS_ERR;
}

int lam_tls_write(lam_tls *session, const unsigned char *buf, size_t len)
{
    int rc = mbedtls_ssl_write(&session->ssl, buf, len);
    if (rc >= 0) {
        return rc;
    }
    if (rc == MBEDTLS_ERR_SSL_WANT_READ || rc == MBEDTLS_ERR_SSL_WANT_WRITE) {
        return LAM_TLS_WANT;
    }
    return LAM_TLS_ERR;
}

/* Writes the peer's end-entity certificate (DER) into `out` when it fits; always returns
 * the full DER length (0 = no certificate available), so a short buffer signals "re-call
 * larger" exactly like the seam's peer_cert contract. */
int lam_tls_peer_cert(lam_tls *session, unsigned char *out, size_t out_len)
{
    const mbedtls_x509_crt *peer = mbedtls_ssl_get_peer_cert(&session->ssl);
    if (peer == NULL || peer->raw.len == 0) {
        return 0;
    }
    if (peer->raw.len <= out_len) {
        memcpy(out, peer->raw.p, peer->raw.len);
    }
    return (int)peer->raw.len;
}

/* Queues close-notify (best effort -- it lands in the Rust outgoing queue for the managed
 * side to flush) and releases the whole session. */
void lam_tls_close(lam_tls *session)
{
    (void)mbedtls_ssl_close_notify(&session->ssl);
    lam_tls_destroy(session);
}
