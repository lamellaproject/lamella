//! The host TLS crypto seam: a PURE BYTE-TRANSFORM behind a trait the embedder supplies (host =
//! rustls and/or mbedTLS; a device = mbedTLS C-link). Unlike the socket seam ([`crate::net`]) this
//! seam NEVER does socket I/O and NEVER blocks -- it is a buffer state machine. The managed
//! `SslStream` does ALL socket I/O through the underlying `NetworkStream` (which already parks the
//! green thread on the reactor), and drives this engine by:

use alloc::boxed::Box;

/// A live TLS session the backend hands out: an index into the backend's own table, opaque to the
/// interpreter (it passes the handle back to identify the session).
pub type TlsHandle = u32;

/// A prepared client/server configuration (roots + verify policy, or the server identity) the
/// backend hands out; a session is created from one with [`client_new`](TlsBackend::client_new) /
/// [`server_new`](TlsBackend::server_new).
pub type TlsConfigHandle = u32;

/// Which TLS engine a configuration selects. On the HOST both stacks are linked and chosen per
/// configuration ("both behind the scenes on the desktop"); on a DEVICE only one is compiled in and
/// this is ignored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TlsStack {
    /// rustls -- pure-Rust, the default host stack.
    Rustls,
    /// mbedTLS -- the embedded-standard stack: the device stack, and a host option that exercises
    /// the exact library shipped on device.
    MbedTls,
}

impl TlsStack {
    /// Decodes the managed stack selector (`0` = rustls, `1` = mbedTLS); anything else falls back to
    /// the default (rustls).
    #[must_use]
    pub fn from_i32(value: i32) -> TlsStack {
        match value {
            1 => TlsStack::MbedTls,
            _ => TlsStack::Rustls,
        }
    }
}

/// How a client trusts the server's certificate chain. Hostname (SNI) verification is on except in
/// [`AcceptAny`](VerifyMode::AcceptAny).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerifyMode {
    /// Use the platform / bundled root store + hostname verification.
    SystemRoots,
    /// Trust exactly the certificate(s) supplied as `roots_pem` (a pinned leaf or CA), plus hostname.
    PinnedCert,
    /// Accept ANY certificate -- TEST ONLY; must be impossible to select in a shipping profile. The
    /// managed `RemoteCertificateValidationCallback` is still invoked, so a test can decide trust in
    /// managed code while the engine itself does not reject the self-signed peer.
    AcceptAny,
}

impl VerifyMode {
    /// Decodes the managed verify-mode selector (`0` = system roots, `1` = pinned cert, `2` = accept
    /// any); anything else is the safe default (system roots).
    #[must_use]
    pub fn from_i32(value: i32) -> VerifyMode {
        match value {
            1 => VerifyMode::PinnedCert,
            2 => VerifyMode::AcceptAny,
            _ => VerifyMode::SystemRoots,
        }
    }
}

/// The state of a TLS session as the pump advances it. The managed side maps these to the integers
/// `0..=3` so the seam crosses as a plain `int`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TlsState {
    /// The handshake is still in progress -- keep pumping (flush `wants_write`, feed more ciphertext).
    Handshaking,
    /// The handshake completed -- application data can flow.
    Established,
    /// The peer sent close-notify (or the session was closed) -- no more plaintext.
    Closed,
    /// The session failed (a handshake/alert/protocol error) -- the managed side throws.
    Error,
}

impl TlsState {
    /// The integer the seam crosses as (mirrored by the managed `SslStream` pump).
    #[must_use]
    pub fn as_i32(self) -> i32 {
        match self {
            TlsState::Handshaking => 0,
            TlsState::Established => 1,
            TlsState::Closed => 2,
            TlsState::Error => 3,
        }
    }
}

/// The TLS crypto seam. `Debug` is a supertrait so the [`crate::interp::Vm`] -- which holds an
/// `Option<Box<dyn TlsBackend>>` -- still derives `Debug`. Every method is a pure buffer transform;
/// none touches a socket or blocks.
pub trait TlsBackend: core::fmt::Debug {
    /// The default stack the managed `SslStream` should select when a program does not request one
    /// (`0` = rustls, `1` = mbedTLS). The host returns its runtime choice ("both behind the scenes on
    /// the desktop"); a device returns the single compiled-in stack. Defaults to rustls.
    fn default_stack(&self) -> i32 {
        0
    }

    /// Builds a CLIENT configuration: which engine, how to trust the peer, and (for
    /// [`VerifyMode::PinnedCert`]) the trusted roots as PEM. Returns a config handle, or `None` if
    /// the configuration is invalid (bad PEM, unsupported stack).
    fn client_config(
        &mut self,
        stack: TlsStack,
        verify: VerifyMode,
        roots_pem: Option<&[u8]>,
    ) -> Option<TlsConfigHandle>;

    /// Builds a SERVER configuration from an identity (a PKCS#12 / PFX blob carrying the certificate
    /// chain + private key, with `password`). Returns a config handle, or `None` if the identity
    /// cannot be loaded.
    fn server_config(
        &mut self,
        stack: TlsStack,
        identity_pfx: &[u8],
        password: &str,
    ) -> Option<TlsConfigHandle>;

    /// Starts a client session from a client config, using `hostname` for SNI + hostname
    /// verification. Returns the session handle, or `None`.
    fn client_new(&mut self, config: TlsConfigHandle, hostname: &str) -> Option<TlsHandle>;

    /// Starts a server session from a server config. Returns the session handle, or `None`.
    fn server_new(&mut self, config: TlsConfigHandle) -> Option<TlsHandle>;

    /// Advances the session state machine over whatever ciphertext has been fed so far (a handshake
    /// step, an alert, application records). Pure -- no I/O.
    fn process(&mut self, tls: TlsHandle) -> TlsState;

    /// Whether the session has outgoing ciphertext queued to send over the socket.
    fn wants_write(&mut self, tls: TlsHandle) -> bool;

    /// Drains queued outgoing ciphertext into `out`, returning the number of bytes written (`0` when
    /// nothing is queued or `out` is empty). The managed side sends these over the socket.
    fn write_tls(&mut self, tls: TlsHandle, out: &mut [u8]) -> usize;

    /// Feeds ciphertext received from the socket, returning how many bytes were consumed (the engine
    /// may buffer less than offered; the managed side re-offers the remainder).
    fn read_tls(&mut self, tls: TlsHandle, input: &[u8]) -> usize;

    /// Reads decrypted application data into `out`. `Some(n)` read `n` plaintext bytes (`0` = none
    /// available yet -- pump + feed more ciphertext); `None` = the peer closed (close-notify).
    fn read_plain(&mut self, tls: TlsHandle, out: &mut [u8]) -> Option<usize>;

    /// Queues application data to encrypt, returning how many bytes were accepted. The managed side
    /// then drains the resulting ciphertext via [`wants_write`](TlsBackend::wants_write) +
    /// [`write_tls`](TlsBackend::write_tls).
    fn write_plain(&mut self, tls: TlsHandle, input: &[u8]) -> usize;

    /// Writes the peer's end-entity certificate (DER) into `out`, returning its full DER length. When
    /// the certificate does not fit, nothing is written and the caller re-calls with a larger buffer;
    /// `0` means no peer certificate is available. Used to drive the managed validation callback.
    fn peer_cert(&mut self, tls: TlsHandle, out: &mut [u8]) -> usize;

    /// Closes a session and releases its handle (sending close-notify where the engine supports it).
    fn close(&mut self, tls: TlsHandle);
}

/// A boxed TLS backend, as the [`crate::interp::Vm`] stores it.
pub type BoxedTlsBackend = Box<dyn TlsBackend>;
