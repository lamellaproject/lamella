/* The Lamella device TLS profile for mbedTLS 3.6 (vendor/mbedtls): a TLS 1.2 CLIENT,
 * sized for a Cortex-M class target.
 *
 * - Client only: MBEDTLS_SSL_CLI_C without MBEDTLS_SSL_SRV_C.
 * - Suites: ECDHE-RSA / ECDHE-ECDSA / RSA key exchange over AES-GCM with SHA-256/384,
 *   curves P-256/P-384 -- the modern-server intersection, no CBC legacy.
 * - No wall clock (MBEDTLS_HAVE_TIME undefined): certificate validity WINDOWS are not
 *   checked on device -- chain signatures and hostname still are. A device wall-clock
 *   source is the documented follow-up.
 * - No filesystem, no built-in networking, no printf-family dependency beyond what the
 *   modules below need: the library runs as a pure byte transform behind the Rust seam.
 * - Memory: every allocation routes through lamella_mbedtls_calloc/free (provided in
 *   Rust over the embedder's allocator); entropy routes through mbedtls_hardware_poll
 *   (the shim forwards to the embedder's source, e.g. the SAM E54 TRNG).
 */
#ifndef LAMELLA_MBEDTLS_CONFIG_H
#define LAMELLA_MBEDTLS_CONFIG_H

#include <stddef.h>

/* The Rust-provided allocator (a size-headered wrapper over the embedder's heap). */
void *lamella_mbedtls_calloc(size_t count, size_t size);
void lamella_mbedtls_free(void *pointer);

/* Platform: custom memory, no OS entropy, everything else default-free. */
#define MBEDTLS_PLATFORM_C
#define MBEDTLS_PLATFORM_MEMORY
#define MBEDTLS_PLATFORM_CALLOC_MACRO lamella_mbedtls_calloc
#define MBEDTLS_PLATFORM_FREE_MACRO lamella_mbedtls_free
#define MBEDTLS_NO_PLATFORM_ENTROPY
#define MBEDTLS_ENTROPY_HARDWARE_ALT

/* RNG: CTR_DRBG seeded from the hardware entropy hook. */
#define MBEDTLS_ENTROPY_C
#define MBEDTLS_CTR_DRBG_C

/* Symmetric crypto + hashes: AES-GCM, SHA-2 family (TLS 1.2 PRF + certificate digests). */
#define MBEDTLS_AES_C
#define MBEDTLS_GCM_C
#define MBEDTLS_CIPHER_C
#define MBEDTLS_MD_C
#define MBEDTLS_SHA224_C
#define MBEDTLS_SHA256_C
#define MBEDTLS_SHA384_C
#define MBEDTLS_SHA512_C

/* Public-key crypto: RSA (PKCS#1 v1.5 + PSS certificate signatures) and ECDHE/ECDSA on
 * the two NIST curves everything on the public internet actually uses. */
#define MBEDTLS_BIGNUM_C
#define MBEDTLS_RSA_C
#define MBEDTLS_PKCS1_V15
#define MBEDTLS_PKCS1_V21
#define MBEDTLS_ECP_C
#define MBEDTLS_ECP_DP_SECP256R1_ENABLED
#define MBEDTLS_ECP_DP_SECP384R1_ENABLED
#define MBEDTLS_ECP_NIST_OPTIM
#define MBEDTLS_ECDH_C
#define MBEDTLS_ECDSA_C

/* ASN.1 / X.509 / PEM: parse the peer chain (info strings removed -- flash). */
#define MBEDTLS_ASN1_PARSE_C
#define MBEDTLS_ASN1_WRITE_C
#define MBEDTLS_OID_C
#define MBEDTLS_PK_C
#define MBEDTLS_PK_PARSE_C
#define MBEDTLS_X509_USE_C
#define MBEDTLS_X509_CRT_PARSE_C
#define MBEDTLS_X509_REMOVE_INFO
#define MBEDTLS_BASE64_C
#define MBEDTLS_PEM_PARSE_C

/* TLS 1.2 client. */
#define MBEDTLS_SSL_TLS_C
#define MBEDTLS_SSL_CLI_C
#define MBEDTLS_SSL_PROTO_TLS1_2
#define MBEDTLS_KEY_EXCHANGE_ECDHE_RSA_ENABLED
#define MBEDTLS_KEY_EXCHANGE_ECDHE_ECDSA_ENABLED
#define MBEDTLS_KEY_EXCHANGE_RSA_ENABLED
#define MBEDTLS_SSL_SERVER_NAME_INDICATION
#define MBEDTLS_SSL_EXTENDED_MASTER_SECRET
#define MBEDTLS_SSL_KEEP_PEER_CERTIFICATE

/* Record buffers: the peer may send full 16 KiB records (no MFL negotiation on the open
 * internet), so the IN buffer takes the standard maximum; we control our own writes, so
 * OUT stays small. This is the dominant per-session RAM cost (~21 KiB together). */
#define MBEDTLS_SSL_IN_CONTENT_LEN 16384
#define MBEDTLS_SSL_OUT_CONTENT_LEN 4096

#endif /* LAMELLA_MBEDTLS_CONFIG_H */
