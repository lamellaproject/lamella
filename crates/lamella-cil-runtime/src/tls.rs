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
    /// engine checks nothing (no parse, no chain walk -- the cheap bench arm); the managed side
    /// reports the chain UNVERIFIED to its validation callback, which makes the trust decision.
    AcceptAny,
    /// Verify chain + hostname like [`SystemRoots`](VerifyMode::SystemRoots) but COMPLETE the
    /// handshake regardless, REPORTING the findings as [`session_flag`] bits -- the
    /// `RemoteCertificateValidationCallback` contract: the callback receives the engine's REAL
    /// policy errors and decides trust. A missing trust source (no registered store) is itself a
    /// finding ([`session_flag::CHAIN_ERRORS`]), never a configuration failure -- a bench peer
    /// with a self-signed certificate reaches the callback with the honest report. The handshake
    /// signature is still hard-verified (the peer proves key possession).
    Report,
}

impl VerifyMode {
    /// Decodes the managed verify-mode selector (`0` = system roots, `1` = pinned cert, `2` = accept
    /// any, `3` = verify-and-report); anything else is the safe default (system roots).
    #[must_use]
    pub fn from_i32(value: i32) -> VerifyMode {
        match value {
            1 => VerifyMode::PinnedCert,
            2 => VerifyMode::AcceptAny,
            3 => VerifyMode::Report,
            _ => VerifyMode::SystemRoots,
        }
    }
}

/// How a backend treats certificate validity WINDOWS -- the `surface.net.tls.clock` capability
/// knob. A board without a real advancing clock (no RTC, no seed, no sync yet) cannot check
/// notBefore/notAfter, and a FROZEN clock is worse than none (it silently trusts a certificate
/// that expires after the frozen instant). So the honest choices are: decide from whether the
/// clock is actually set (the default), fail closed, or skip the date check LOUDLY.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ClockPolicy {
    /// The default: resolved PER SESSION, at creation, from whether the backend's wall clock is
    /// SET. Set (an RTC, a seed, or a synced `SystemClock` behind the backend's time source) ->
    /// validity windows are checked in FULL, exactly like [`Require`](ClockPolicy::Require) -- a
    /// board with a trusted clock is never weakened. Never set (the time source reads 0, the
    /// epoch) -> the window is tolerated and RECORDED like
    /// [`SkipWithWarning`](ClockPolicy::SkipWithWarning), so a clockless board can still
    /// bootstrap (the clock ladder's leap-of-faith rung) -- and the first session created AFTER
    /// a sync is full-strength automatically.
    #[default]
    Adaptive,
    /// FORCE full validation: validity windows are checked against the embedder's clock; with no
    /// clock (the epoch) a REQUIRED verify fails closed, loudly -- even clockless. The hardening
    /// override for a deployment that would rather not connect than tolerate an unchecked window.
    Require,
    /// FORCE the date skip (testing / explicit opt-in): the date-window errors (`expired` /
    /// `not yet valid`) are cleared during verification even when a clock is set, while the chain
    /// signatures and hostname are still HARD-checked, and the session RECORDS that dates went
    /// unchecked -- surfaced to managed as a policy-error bit
    /// ([`session_flag::DATES_UNCHECKED`]) so the app is told "certificate dates were not
    /// verified" and decides. NEVER a silent accept.
    SkipWithWarning,
}

impl ClockPolicy {
    /// Decodes the managed clock-policy selector: `1` = force skip-with-warning, `2` = force
    /// require; anything else -- including an unset knob's `0` -- is the adaptive default.
    #[must_use]
    pub fn from_i32(value: i32) -> ClockPolicy {
        match value {
            1 => ClockPolicy::SkipWithWarning,
            2 => ClockPolicy::Require,
            _ => ClockPolicy::Adaptive,
        }
    }
}

/// A TLS protocol version, for the `surface.net.tls.version` pin. Ordered oldest-to-newest so a
/// `min <= max` range is a simple comparison; SSL/3.0 and TLS 1.0/1.1 are deliberately absent (no
/// backend offers them -- the modern-server floor is TLS 1.2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum TlsVersion {
    /// TLS 1.2.
    Tls12,
    /// TLS 1.3.
    Tls13,
}

impl TlsVersion {
    /// Decodes the managed version selector (`12` = TLS 1.2, `13` = TLS 1.3); anything else is the
    /// safe modern floor (TLS 1.2).
    #[must_use]
    pub fn from_i32(value: i32) -> TlsVersion {
        match value {
            13 => TlsVersion::Tls13,
            _ => TlsVersion::Tls12,
        }
    }
}

/// The allowed TLS protocol-version window -- the `surface.net.tls.version` capability knob. The
/// firmware/host sets it to HARDEN which versions the backend will negotiate; a backend clamps it to
/// what it actually supports (and fails a configuration loudly when the pin demands a version it
/// cannot provide -- e.g. a TLS 1.3-only pin on the TLS-1.2-only device engine). The default admits
/// the whole modern range (TLS 1.2 through 1.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TlsVersionRange {
    /// The oldest version the backend may negotiate.
    pub min: TlsVersion,
    /// The newest version the backend may negotiate.
    pub max: TlsVersion,
}

impl Default for TlsVersionRange {
    fn default() -> TlsVersionRange {
        TlsVersionRange { min: TlsVersion::Tls12, max: TlsVersion::Tls13 }
    }
}

impl TlsVersionRange {
    /// A range admitting exactly one version.
    #[must_use]
    pub fn only(version: TlsVersion) -> TlsVersionRange {
        TlsVersionRange { min: version, max: version }
    }

    /// Whether the range is well-formed (`min <= max`) and thus admits at least one version.
    #[must_use]
    pub fn is_valid(self) -> bool {
        self.min <= self.max
    }

    /// Whether `version` falls within the range.
    #[must_use]
    pub fn admits(self, version: TlsVersion) -> bool {
        self.min <= version && version <= self.max
    }
}

/// Post-handshake session flags a backend reports through [`TlsBackend::session_flags`]. Each is a
/// bit the managed `SslStream` reads once the handshake completes, to surface trust caveats the
/// engine tolerated (rather than rejected) under a capability knob.
pub mod session_flag {
    /// The peer certificate's validity DATES were not verified -- the
    /// [`super::ClockPolicy::SkipWithWarning`] force, or the default
    /// [`super::ClockPolicy::Adaptive`] while the clock was never set, cleared an `expired` /
    /// `not yet valid` error the clockless board could not check. The chain signatures and
    /// hostname WERE verified; only the window was skipped. Managed maps this to a
    /// `RemoteCertificateChainErrors` policy error so a validation callback (or the app) decides.
    pub const DATES_UNCHECKED: i32 = 1 << 0;
    /// [`super::VerifyMode::Report`] found the chain UNTRUSTWORTHY (untrusted or missing
    /// issuer, a bad signature, a date failure under a real clock, no usable trust source) --
    /// or could not verify it at all. Managed maps this to `RemoteCertificateChainErrors`; the
    /// validation callback decides trust.
    pub const CHAIN_ERRORS: i32 = 1 << 1;
    /// [`super::VerifyMode::Report`] found the peer certificate valid for some name(s) but NOT
    /// the requested hostname. Managed maps this to `RemoteCertificateNameMismatch`.
    pub const NAME_MISMATCH: i32 = 1 << 2;
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

    /// The post-handshake [`session_flag`] bits for the session: trust caveats the engine TOLERATED
    /// under a capability knob (rather than rejecting), which the managed side surfaces. `0` = a clean
    /// handshake with nothing to flag. Defaults to `0` for a backend with no such tolerances.
    fn session_flags(&mut self, _tls: TlsHandle) -> i32 {
        0
    }

    /// Like [`TlsBackend::client_config`] but the configuration OFFERS `alpn` (each entry one
    /// protocol name, e.g. `b"ntske/1"`) in the handshake's ALPN extension. Default: `None` --
    /// "this backend cannot offer ALPN" -- so a protocol that REQUIRES ALPN (RFC 8915 NTS-KE)
    /// fails loudly at configuration on a backend without it, rather than handshaking degraded.
    fn client_config_alpn(
        &mut self,
        _stack: TlsStack,
        _verify: VerifyMode,
        _roots_pem: Option<&[u8]>,
        _alpn: &[&[u8]],
    ) -> Option<TlsConfigHandle> {
        None
    }

    /// Like [`TlsBackend::server_config`] but the configuration ACCEPTS the given ALPN protocols
    /// (the conformance tests' NTS-KE-shaped server; a backend without ALPN keeps the `None`
    /// default).
    fn server_config_alpn(
        &mut self,
        _stack: TlsStack,
        _identity_pfx: &[u8],
        _password: &str,
        _alpn: &[&[u8]],
    ) -> Option<TlsConfigHandle> {
        None
    }

    /// Whether the established session negotiated exactly `protocol` via ALPN -- the RFC 8915
    /// "the client MUST verify ntske/1 was selected" check. `false` when nothing was negotiated,
    /// a different protocol won, or the backend does not track ALPN.
    fn alpn_is(&mut self, _tls: TlsHandle, _protocol: &[u8]) -> bool {
        false
    }

    /// RFC 5705 / RFC 8446 keying-material export: fills `out` from the established session's
    /// secrets under `label` + `context`. The bytes stay RUNTIME-side -- the managed tier only
    /// ever receives a native key HANDLE minted from them (the `Vm` native-key store), so
    /// exported keys never appear in managed values. `false` = not established, or the backend
    /// cannot export (the caller fails loudly rather than proceed unkeyed).
    fn export_keying_material(
        &mut self,
        _tls: TlsHandle,
        _out: &mut [u8],
        _label: &[u8],
        _context: Option<&[u8]>,
    ) -> bool {
        false
    }

    /// Closes a session and releases its handle (sending close-notify where the engine supports it).
    fn close(&mut self, tls: TlsHandle);
}

/// A boxed TLS backend, as the [`crate::interp::Vm`] stores it.
pub type BoxedTlsBackend = Box<dyn TlsBackend>;
