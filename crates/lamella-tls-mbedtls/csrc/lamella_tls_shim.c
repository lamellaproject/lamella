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
#include <time.h> /* struct tm, for the libc-free mbedtls_platform_gmtime_r below */

#include <mbedtls/aes.h>
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

/* Provided by src/lib.rs: the `index`-th bundled system root whose SUBJECT DN equals the
 * `issuer` DN (the raw bytes of a child certificate's issuer Name), or NULL when there are no
 * more matches. `*out_len` receives the DER length. The lazy trusted-cert callback walks this
 * to gather only the candidate roots for the chain it is verifying. */
extern const unsigned char *lamella_tls_root_der(
    const unsigned char *issuer, size_t issuer_len, size_t index, size_t *out_len);
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

/* Provided by src/lib.rs: the embedder's wall clock in Unix seconds, or 0 when no source is
 * registered (which pre-dates every real certificate -> a REQUIRED verify fails closed). */
extern long long lamella_tls_time(void);

/* MBEDTLS_PLATFORM_TIME_MACRO: mbedTLS reads the clock here for certificate validity. */
long long lamella_mbedtls_time(long long *t)
{
    long long now = lamella_tls_time();
    if (t != NULL) {
        *t = now;
    }
    return now;
}

/* MBEDTLS_PLATFORM_MS_TIME_ALT: a millisecond clock for the DTLS / TLS-1.3 retransmit
 * timers (unused on this TLS 1.2 client, but HAVE_TIME compiles the reference). Derived
 * from the same wall clock -- coarse (whole seconds), which is irrelevant while unused. */
mbedtls_ms_time_t mbedtls_ms_time(void)
{
    return (mbedtls_ms_time_t)lamella_tls_time() * 1000;
}

/* MBEDTLS_PLATFORM_GMTIME_R_ALT: a libc-free UTC breakdown of Unix seconds -- the x509 date
 * check converts the current time to y/m/d/h/m/s and compares it with the certificate's
 * notBefore/notAfter. Days->civil is Howard Hinnant's proleptic-Gregorian algorithm (exact,
 * branch-light, no tables); only the fields the x509 comparison reads are filled. */
struct tm *mbedtls_platform_gmtime_r(const mbedtls_time_t *tt, struct tm *tm_buf)
{
    long long secs = (long long)*tt;
    long long days = secs / 86400;
    long long rem = secs % 86400;
    if (rem < 0) {
        rem += 86400;
        days -= 1;
    }
    tm_buf->tm_hour = (int)(rem / 3600);
    tm_buf->tm_min = (int)((rem % 3600) / 60);
    tm_buf->tm_sec = (int)(rem % 60);

    long long z = days + 719468; /* shift the epoch to 0000-03-01 */
    long long era = (z >= 0 ? z : z - 146096) / 146097;
    unsigned doe = (unsigned)(z - era * 146097);                       /* [0, 146096] */
    unsigned yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; /* [0, 399] */
    long long y = (long long)yoe + era * 400;
    unsigned doy = doe - (365 * yoe + yoe / 4 - yoe / 100); /* [0, 365] */
    unsigned mp = (5 * doy + 2) / 153;                      /* [0, 11] (Mar=0) */
    unsigned d = doy - (153 * mp + 2) / 5 + 1;              /* [1, 31] */
    unsigned m = mp < 10 ? mp + 3 : mp - 9;                 /* [1, 12] */
    y += (m <= 2);

    tm_buf->tm_year = (int)(y - 1900);
    tm_buf->tm_mon = (int)(m - 1);
    tm_buf->tm_mday = (int)d;
    tm_buf->tm_wday = 0; /* the x509 window comparison reads neither wday nor yday */
    tm_buf->tm_yday = 0;
    tm_buf->tm_isdst = 0;
    return tm_buf;
}

typedef struct lam_tls {
    mbedtls_ssl_context ssl;
    mbedtls_ssl_config conf;
    mbedtls_ctr_drbg_context drbg;
    mbedtls_entropy_context entropy;
    mbedtls_x509_crt ca;
    void *user;
    int have_ca;
    /* Set to 1 by the skip-with-warning verify callback when it cleared a date-window error
     * (expired / not-yet-valid) the clockless board could not check. Read post-handshake via
     * lam_tls_dates_skipped so the managed side surfaces it as a policy-error warning. */
    int dates_skipped;
} lam_tls;

/* The verify callback for the surface.net.tls.clock=skip-with-warning policy: clears ONLY the
 * date-window failures (expired / not yet valid) while leaving every other flag (bad chain
 * signature, untrusted root, hostname mismatch) intact for mbedTLS to reject. Records that it
 * did, so the caveat is never silent. Runs per chain element; depth is unused (a date error at
 * any depth is equally unverifiable without a clock). */
static int lam_vrfy_skip_dates(void *ctx, mbedtls_x509_crt *crt, int depth, uint32_t *flags)
{
    (void)crt;
    (void)depth;
    lam_tls *session = (lam_tls *)ctx;
    uint32_t dated = *flags & (MBEDTLS_X509_BADCERT_EXPIRED | MBEDTLS_X509_BADCERT_FUTURE);
    if (dated != 0) {
        session->dates_skipped = 1;
        *flags &= ~dated;
    }
    return 0;
}

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

/* The lazy trusted-certificate callback (mbedtls_ssl_conf_ca_cb) for system-root trust: given
 * the peer chain's `child`, gather the bundled root(s) whose subject DN matches its issuer DN
 * and hand them to mbedTLS as freshly-parsed candidates. mbedTLS verifies the signature against
 * them and frees the list afterward. Parsing only the matching roots (usually one) keeps the
 * device off parsing the whole ~120-root store into its small pool. Returns 0 with a (possibly
 * empty) list; a genuine allocation failure returns an mbedTLS error so the handshake fails
 * closed rather than trusting nothing silently. */
static int lam_ca_cb(void *ctx, const mbedtls_x509_crt *child, mbedtls_x509_crt **candidates)
{
    (void)ctx;
    mbedtls_x509_crt *list = (mbedtls_x509_crt *)mbedtls_calloc(1, sizeof(mbedtls_x509_crt));
    if (list == NULL) {
        return MBEDTLS_ERR_X509_ALLOC_FAILED;
    }
    mbedtls_x509_crt_init(list);
    for (size_t index = 0;; index++) {
        size_t der_len = 0;
        const unsigned char *der =
            lamella_tls_root_der(child->issuer_raw.p, child->issuer_raw.len, index, &der_len);
        if (der == NULL || der_len == 0) {
            break;
        }
        /* A single bad root DER should not abort the whole lookup: skip it and keep going. */
        (void)mbedtls_x509_crt_parse_der(list, der, der_len);
    }
    *candidates = list;
    return 0;
}

/* Builds a client session. verify_mode: 0 = required against ca_pem (a NUL-terminated PEM
 * bundle), 1 = accept-any (the managed validation callback owns the trust decision; the
 * handshake still proves key possession), 2 = required against the bundled SYSTEM ROOTS via the
 * lazy ca_cb (ca_pem unused), 3 = verify-and-REPORT (OPTIONAL authmode against the bundled
 * system roots when registered: chain + hostname + dates are checked, the handshake completes
 * regardless, and the findings are read post-handshake via lam_tls_report_flags -- the managed
 * validation callback receives them and decides trust). `skip_dates` (meaningful with
 * verify_mode 0, 2 or 3) installs the clock-skip verify callback: the chain + hostname are
 * still checked, only the validity WINDOW is tolerated (the surface.net.tls.clock policy).
 * `hostname` drives SNI + name verification. Returns NULL on any setup failure. */
lam_tls *lam_tls_client_new(
    const unsigned char *ca_pem,
    size_t ca_len,
    const char *hostname,
    int verify_mode,
    int skip_dates,
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
    session->dates_skipped = 0;

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
        if (skip_dates) {
            /* Clock-skip policy: tolerate ONLY the date window; the callback still lets
             * mbedTLS reject a bad signature / untrusted root / hostname mismatch. */
            mbedtls_ssl_conf_verify(&session->conf, lam_vrfy_skip_dates, session);
        }
    } else if (verify_mode == 2) {
        /* System-root trust: the lazy ca_cb supplies only the matching bundled root(s) per
         * chain, so the whole store never enters the pool. Still VERIFY_REQUIRED. */
        mbedtls_ssl_conf_ca_cb(&session->conf, lam_ca_cb, session);
        mbedtls_ssl_conf_authmode(&session->conf, MBEDTLS_SSL_VERIFY_REQUIRED);
        if (skip_dates) {
            mbedtls_ssl_conf_verify(&session->conf, lam_vrfy_skip_dates, session);
        }
    } else if (verify_mode == 3) {
        /* Verify-and-report: OPTIONAL authmode completes the handshake regardless of the
         * verification outcome; the findings are read post-handshake via lam_tls_report_flags
         * and the managed callback decides trust. The trust source is the supplied ca_pem when
         * given, else the lazy system-root lookup (an unregistered store just yields an empty
         * candidate list -> NOT_TRUSTED recorded, never a setup failure). */
        if (ca_pem != NULL && ca_len != 0) {
            if (mbedtls_x509_crt_parse(&session->ca, ca_pem, ca_len) != 0) {
                lam_tls_destroy(session);
                return NULL;
            }
            session->have_ca = 1;
            mbedtls_ssl_conf_ca_chain(&session->conf, &session->ca, NULL);
        } else {
            mbedtls_ssl_conf_ca_cb(&session->conf, lam_ca_cb, session);
        }
        mbedtls_ssl_conf_authmode(&session->conf, MBEDTLS_SSL_VERIFY_OPTIONAL);
        if (skip_dates) {
            mbedtls_ssl_conf_verify(&session->conf, lam_vrfy_skip_dates, session);
        }
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

/* Whether the skip-with-warning callback cleared a validity-window error during the handshake
 * (1 = the peer chain had an expired / not-yet-valid certificate the clockless board could not
 * check, and it was tolerated). Read post-handshake to surface the caveat to managed. */
int lam_tls_dates_skipped(lam_tls *session)
{
    return session->dates_skipped;
}

/* Post-handshake trust findings for the verify-and-report mode (verify_mode 3), mapped to the
 * seam's session-flag bits: 2 = chain errors, 4 = hostname mismatch (mirroring the Rust
 * session_flag constants). 0 = the chain verified clean. Meaningful once the handshake
 * completed on a report-mode session; the Rust side gates the call on the stored mode.
 * 0xFFFFFFFF ("no result available") maps to chain-errors: an unverified chain is a finding. */
int lam_tls_report_flags(lam_tls *session)
{
    uint32_t result = mbedtls_ssl_get_verify_result(&session->ssl);
    if (result == 0) {
        return 0;
    }
    if (result == 0xFFFFFFFFu) {
        return 2;
    }
    int flags = 0;
    if (result & MBEDTLS_X509_BADCERT_CN_MISMATCH) {
        flags |= 4;
    }
    if (result & ~(uint32_t)MBEDTLS_X509_BADCERT_CN_MISMATCH) {
        flags |= 2;
    }
    return flags;
}

/* Queues close-notify (best effort -- it lands in the Rust outgoing queue for the managed
 * side to flush) and releases the whole session. */
void lam_tls_close(lam_tls *session)
{
    (void)mbedtls_ssl_close_notify(&session->ssl);
    lam_tls_destroy(session);
}

/* --- The AEAD composition's AES block primitive ---------------------------------------- */

/* A bare AES-128 ENCRYPT-block context for the SIV composition: the Rust side builds
 * CMAC / S2V / CTR over this one primitive (RFC 5297; the appendix vectors pin the
 * composition byte-exact against the host backend). Only MBEDTLS_AES_C is needed --
 * no cipher layer, no CMAC module -- so the device build stays minimal. */
void *lam_aes128_new(const unsigned char key[16])
{
    mbedtls_aes_context *aes =
        (mbedtls_aes_context *)mbedtls_calloc(1, sizeof(mbedtls_aes_context));
    if (aes == NULL) {
        return NULL;
    }
    mbedtls_aes_init(aes);
    if (mbedtls_aes_setkey_enc(aes, key, 128) != 0) {
        mbedtls_aes_free(aes);
        mbedtls_free(aes);
        return NULL;
    }
    return aes;
}

void lam_aes128_encrypt_block(void *ctx, const unsigned char in[16], unsigned char out[16])
{
    (void)mbedtls_aes_crypt_ecb((mbedtls_aes_context *)ctx, MBEDTLS_AES_ENCRYPT, in, out);
}

void lam_aes128_free(void *ctx)
{
    if (ctx != NULL) {
        mbedtls_aes_free((mbedtls_aes_context *)ctx);
        mbedtls_free(ctx);
    }
}
