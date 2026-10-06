// Lamella managed corlib (from scratch). -- System.Net.Security.SslStream
#if LAMELLA_SURFACE_NET_TLS
using System.IO;
using System.Security.Authentication;
using System.Security.Cryptography.X509Certificates;

namespace System.Net.Security
{
#if LAMELLA_NET_2_0
    public
#else
    internal
#endif
    class SslStream : Stream
    {
        private const int TlsBufferSize = 16640;
        private const int PeerCertBufferSize = 2048;
        private int _stack;
        private const int VerifySystemRoots = 0;
        private const int VerifyAcceptAny = 2;
        private const int VerifyReport = 3;
        private const int StateEstablished = 1;
        private const int StateError = 3;
        private const int PlainClosed = -2;
        private const int PlainFailed = -1;
        private const int FlagDatesUnchecked = 1;
        private const int FlagChainErrors = 2;
        private const int FlagNameMismatch = 4;
        private const int FlagReportPresent = 8;

        private Stream _inner;
        private bool _leaveInnerStreamOpen;
        private RemoteCertificateValidationCallback _validationCallback;
        private bool _acceptAnyCertificate;
        private int _tls;
        private bool _authenticated;
        private byte[] _xfer;

        public SslStream(Stream innerStream) : this(innerStream, false, null) { }

        public SslStream(Stream innerStream, bool leaveInnerStreamOpen)
            : this(innerStream, leaveInnerStreamOpen, null) { }

#if LAMELLA_NET_2_0
        public
#else
        internal
#endif
        SslStream(
            Stream innerStream,
            bool leaveInnerStreamOpen,
            RemoteCertificateValidationCallback userCertificateValidationCallback)
        {
            _inner = innerStream;
            _leaveInnerStreamOpen = leaveInnerStreamOpen;
            _validationCallback = userCertificateValidationCallback;
            _tls = -1;
            _stack = TlsNative.DefaultStack();
        }

        public bool IsAuthenticated { get { return _authenticated; } }
        public bool IsEncrypted { get { return _authenticated; } }

        public bool AcceptAnyCertificate
        {
            get { return _acceptAnyCertificate; }
            set { _acceptAnyCertificate = value; }
        }

        public void AuthenticateAsClient(string targetHost)
        {
            AuthenticateClient(targetHost, null, null, false);
        }

#if LAMELLA_NET_2_0
        /// <summary>
        /// Authenticates the client side of the connection to <paramref name="targetHost"/>, presenting a
        /// certificate from <paramref name="clientCertificates"/> if the server asks for one: the first
        /// that carries its private key, as one made by <c>X509Certificate2.CreateFromPem</c> does. With no
        /// such certificate the client presents none, and a server that requires one refuses the handshake.
        /// </summary>
        /// <param name="targetHost">The server's name, sent in the handshake and checked against its certificate.</param>
        /// <param name="clientCertificates">The certificates the client may present; may be <see langword="null"/>.</param>
        /// <param name="enabledSslProtocols">
        /// The protocol versions to allow, or <see cref="SslProtocols.None"/> for the TLS engine's own choice.
        /// Every TLS engine this runtime has speaks TLS 1.2, so a value that does not allow it is refused.
        /// </param>
        /// <param name="checkCertificateRevocation">
        /// Whether the server certificate's revocation status must be known. This runtime's TLS engines do
        /// not check revocation, so <see langword="true"/> reports the status as unknown: a
        /// <see cref="SslPolicyErrors.RemoteCertificateChainErrors"/> a validation callback can accept,
        /// and without one the handshake fails.
        /// </param>
        /// <exception cref="AuthenticationException">
        /// The handshake failed: the server sent a fatal alert (named in the message, as .NET names it), the
        /// server's certificate was not trusted, or no allowed protocol version is one the engine speaks.
        /// </exception>
        public void AuthenticateAsClient(
            string targetHost,
            X509CertificateCollection clientCertificates,
            SslProtocols enabledSslProtocols,
            bool checkCertificateRevocation)
        {
            if (enabledSslProtocols != SslProtocols.None && ((int)enabledSslProtocols & 3072) == 0)
            {
                throw new AuthenticationException(
                    "None of the enabled protocol versions is TLS 1.2, the version this TLS engine speaks.");
            }
            byte[] chain = null;
            byte[] key = null;
            if ((object)clientCertificates != null)
            {
                for (int i = 0; i < clientCertificates.Count; i++)
                {
                    X509Certificate2 candidate = clientCertificates[i] as X509Certificate2;
                    if ((object)candidate != null && (object)candidate.GetClientKeyPem() != null)
                    {
                        chain = candidate.RawData;
                        key = candidate.GetClientKeyPem();
                        break;
                    }
                }
            }
            AuthenticateClient(targetHost, chain, key, checkCertificateRevocation);
        }
#endif

        private const int IdentityUnsupported = -2;
        private const int IdentityCertificateUnreadable = -3;
        private const int IdentityKeyUnreadable = -4;
        private const int IdentityKeyMismatch = -5;

        private void AuthenticateClient(string targetHost, byte[] chain, byte[] key, bool revocationRequested)
        {
            _xfer = new byte[TlsBufferSize];
            int verifyMode = VerifySystemRoots;
            if (_acceptAnyCertificate) verifyMode = VerifyAcceptAny;
            else if ((object)_validationCallback != null) verifyMode = VerifyReport;
            int config;
            if ((object)chain == null)
            {
                config = TlsNative.ClientConfig(_stack, verifyMode, null);
            }
            else
            {
                config = TlsNative.ClientConfigIdentity(_stack, verifyMode, null, chain, key);
                if (config == IdentityUnsupported)
                    throw new AuthenticationException("This TLS engine cannot present a client certificate.");
                if (config == IdentityCertificateUnreadable)
                    throw new AuthenticationException("The client certificate could not be read.");
                if (config == IdentityKeyUnreadable)
                    throw new AuthenticationException("The client certificate's private key could not be read.");
                if (config == IdentityKeyMismatch)
                    throw new AuthenticationException("The client certificate's private key is not the certificate's.");
            }
            if (config < 0) throw new AuthenticationException("Could not build the TLS client configuration.");
            _tls = TlsNative.ClientNew(config, targetHost);
            if (_tls < 0) throw new AuthenticationException("Could not start the TLS client session.");
            Handshake();
            int flags = TlsNative.SessionFlags(_tls);
            SslPolicyErrors errors = SslPolicyErrors.None;
            if ((flags & FlagReportPresent) == 0)
            {
                errors |= SslPolicyErrors.RemoteCertificateChainErrors;
            }
            if ((flags & (FlagDatesUnchecked | FlagChainErrors)) != 0)
            {
                errors |= SslPolicyErrors.RemoteCertificateChainErrors;
            }
            if ((flags & FlagNameMismatch) != 0)
            {
                errors |= SslPolicyErrors.RemoteCertificateNameMismatch;
            }
            if (_acceptAnyCertificate)
            {
                errors |= SslPolicyErrors.RemoteCertificateChainErrors;
            }
            if (revocationRequested)
            {
                errors |= SslPolicyErrors.RemoteCertificateChainErrors;
            }
            if ((object)_validationCallback != null)
            {
                X509Certificate peer = GetPeerCertificate();
                if ((object)peer == null)
                {
                    errors |= SslPolicyErrors.RemoteCertificateNotAvailable;
                }
                bool accepted = _validationCallback(this, peer, new X509Chain(), errors);
                if (!accepted)
                {
                    Close();
                    throw new AuthenticationException("The remote certificate was rejected by the validation callback.");
                }
            }
            else if (errors != SslPolicyErrors.None && !_acceptAnyCertificate)
            {
                Close();
                if (revocationRequested)
                {
                    throw new AuthenticationException(
                        "The remote certificate's revocation status could not be checked, and no validation callback accepted it.");
                }
                throw new AuthenticationException(
                    "The remote certificate's validity dates could not be verified (no clock) and no validation callback accepted them.");
            }
            _authenticated = true;
        }

#if LAMELLA_NET_2_0
        public void AuthenticateAsServer(X509Certificate serverCertificate)
        {
            _xfer = new byte[TlsBufferSize];
            byte[] identity;
            string password;
            X509Certificate2 identityCert = serverCertificate as X509Certificate2;
            if ((object)identityCert != null)
            {
                identity = identityCert.GetIdentityBytes();
                password = identityCert.GetIdentityPassword();
            }
            else
            {
                identity = serverCertificate.GetRawCertData();
                password = "";
            }
            int config = TlsNative.ServerConfig(_stack, identity, password);
            if (config < 0) throw new AuthenticationException("Could not build the TLS server configuration.");
            _tls = TlsNative.ServerNew(config);
            if (_tls < 0) throw new AuthenticationException("Could not start the TLS server session.");
            Handshake();
            _authenticated = true;
        }
#endif

        private void Handshake()
        {
            while (true)
            {
                int state = TlsNative.Process(_tls);
                FlushOutgoing();
                if (state == StateEstablished) return;
                if (state == StateError) throw HandshakeFailed();
                int received = _inner.Read(_xfer, 0, _xfer.Length);
                if (received <= 0) throw new AuthenticationException("The connection closed during the TLS handshake.");
                FeedIncoming(received);
            }
        }

        private AuthenticationException HandshakeFailed()
        {
            int alert = TlsNative.PeerAlert(_tls);
            if (alert <= 0) return new AuthenticationException("The TLS handshake failed.");
            return new AuthenticationException(
                "Authentication failed because the remote party sent a TLS alert: '" + AlertName(alert) + "'.");
        }

        private static string AlertName(int alert)
        {
            switch (alert)
            {
                case 10: return "UnexpectedMessage";
                case 20: return "BadRecordMac";
                case 21: return "DecryptionFailed";
                case 22: return "RecordOverflow";
                case 30: return "DecompressionFail";
                case 40: return "HandshakeFailure";
                case 42: return "BadCertificate";
                case 43: return "UnsupportedCert";
                case 44: return "CertificateRevoked";
                case 45: return "CertificateExpired";
                case 46: return "CertificateUnknown";
                case 47: return "IllegalParameter";
                case 48: return "UnknownCA";
                case 49: return "AccessDenied";
                case 50: return "DecodeError";
                case 51: return "DecryptError";
                case 60: return "ExportRestriction";
                case 70: return "ProtocolVersion";
                case 71: return "InsuffientSecurity";
                case 80: return "InternalError";
                case 90: return "UserCanceled";
                case 100: return "NoRenegotiation";
                case 110: return "UnsupportedExt";
                default: return alert.ToString();
            }
        }

        private void FlushOutgoing()
        {
            while (TlsNative.WantsWrite(_tls) != 0)
            {
                int produced = TlsNative.WriteTls(_tls, _xfer, 0, _xfer.Length);
                if (produced <= 0) break;
                _inner.Write(_xfer, 0, produced);
            }
            _inner.Flush();
        }

        private int FeedIncoming(int count)
        {
            int fed = 0;
            int state = StateEstablished;
            while (fed < count)
            {
                int consumed = TlsNative.ReadTls(_tls, _xfer, fed, count - fed);
                fed += consumed;
                state = TlsNative.Process(_tls);
                if (consumed == 0) break;
            }
            return state;
        }

        private static IOException SessionFailed()
        {
            return new IOException("The TLS session failed, so the data received may be incomplete.");
        }

        private X509Certificate GetPeerCertificate()
        {
            byte[] probe = new byte[PeerCertBufferSize];
            int length = TlsNative.PeerCert(_tls, probe);
            if (length <= 0) return null;
            if (length > probe.Length)
            {
                probe = new byte[length];
                length = TlsNative.PeerCert(_tls, probe);
                if (length <= 0 || length > probe.Length) return null;
            }
            byte[] der = new byte[length];
            Array.Copy(probe, der, length);
            return new X509Certificate(der);
        }

        public override bool CanRead { get { return _authenticated; } }
        public override bool CanWrite { get { return _authenticated; } }
        public override bool CanSeek { get { return false; } }
        public override long Length { get { throw new NotSupportedException(); } }
        public override long Position
        {
            get { throw new NotSupportedException(); }
            set { throw new NotSupportedException(); }
        }

        public override int Read(byte[] buffer, int offset, int count)
        {
            if (!_authenticated) throw new InvalidOperationException("The stream is not authenticated.");
            while (true)
            {
                int plain = TlsNative.ReadPlain(_tls, buffer, offset, count);
                if (plain > 0) return plain;
                if (plain == PlainClosed) return 0;
                if (plain == PlainFailed) throw SessionFailed();
                FlushOutgoing();
                int received = _inner.Read(_xfer, 0, _xfer.Length);
                if (received <= 0) return 0;
                if (FeedIncoming(received) == StateError) throw SessionFailed();
            }
        }

        public override void Write(byte[] buffer, int offset, int count)
        {
            if (!_authenticated) throw new InvalidOperationException("The stream is not authenticated.");
            int written = 0;
            bool stalled = false;
            while (written < count)
            {
                int queued = TlsNative.WritePlain(_tls, buffer, offset + written, count - written);
                if (queued < 0) throw SessionFailed();
                written += queued;
                FlushOutgoing();
                if (queued == 0)
                {
                    if (stalled) throw new IOException("The TLS engine took none of the data to write.");
                    stalled = true;
                }
                else
                {
                    stalled = false;
                }
            }
        }

        public override void Flush() { _inner.Flush(); }
        public override long Seek(long offset, SeekOrigin origin) { throw new NotSupportedException(); }
        public override void SetLength(long value) { throw new NotSupportedException(); }

        protected override void Dispose(bool disposing)
        {
            if (_tls >= 0)
            {
                if (disposing && _authenticated)
                {
                    TlsNative.CloseNotify(_tls);
                    try
                    {
                        FlushOutgoing();
                    }
                    catch (IOException)
                    {
                    }
                    catch (ObjectDisposedException)
                    {
                    }
                }
                TlsNative.CloseTls(_tls);
                _tls = -1;
            }
            if (disposing && !_leaveInnerStreamOpen) _inner.Close();
            base.Dispose(disposing);
        }
    }
}
#endif
