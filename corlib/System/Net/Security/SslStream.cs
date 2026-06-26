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
        private const int PeerCertBufferSize = 8192;
        private int _stack;
        private const int VerifySystemRoots = 0;
        private const int VerifyAcceptAny = 2;
        private const int StateEstablished = 1;
        private const int StateError = 3;
        private const int PlainClosed = -2;

        private Stream _inner;
        private bool _leaveInnerStreamOpen;
        private RemoteCertificateValidationCallback _validationCallback;
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

        public void AuthenticateAsClient(string targetHost)
        {
            _xfer = new byte[TlsBufferSize];
            int verifyMode = (object)_validationCallback != null ? VerifyAcceptAny : VerifySystemRoots;
            int config = TlsNative.ClientConfig(_stack, verifyMode, null);
            if (config < 0) throw new AuthenticationException("Could not build the TLS client configuration.");
            _tls = TlsNative.ClientNew(config, targetHost);
            if (_tls < 0) throw new AuthenticationException("Could not start the TLS client session.");
            Handshake();
            if ((object)_validationCallback != null)
            {
                X509Certificate peer = GetPeerCertificate();
                bool accepted = _validationCallback(
                    this, peer, new X509Chain(), SslPolicyErrors.RemoteCertificateChainErrors);
                if (!accepted)
                {
                    Close();
                    throw new AuthenticationException("The remote certificate was rejected by the validation callback.");
                }
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
                if (state == StateError) throw new AuthenticationException("The TLS handshake failed.");
                int received = _inner.Read(_xfer, 0, _xfer.Length);
                if (received <= 0) throw new AuthenticationException("The connection closed during the TLS handshake.");
                FeedIncoming(received);
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

        private void FeedIncoming(int count)
        {
            int fed = 0;
            while (fed < count)
            {
                int consumed = TlsNative.ReadTls(_tls, _xfer, fed, count - fed);
                fed += consumed;
                TlsNative.Process(_tls);
                if (consumed == 0) break;
            }
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
                FlushOutgoing();
                int received = _inner.Read(_xfer, 0, _xfer.Length);
                if (received <= 0) return 0;
                FeedIncoming(received);
            }
        }

        public override void Write(byte[] buffer, int offset, int count)
        {
            if (!_authenticated) throw new InvalidOperationException("The stream is not authenticated.");
            int written = 0;
            while (written < count)
            {
                int queued = TlsNative.WritePlain(_tls, buffer, offset + written, count - written);
                written += queued;
                FlushOutgoing();
                if (queued == 0) break;
            }
        }

        public override void Flush() { _inner.Flush(); }
        public override long Seek(long offset, SeekOrigin origin) { throw new NotSupportedException(); }
        public override void SetLength(long value) { throw new NotSupportedException(); }

        public override void Close()
        {
            if (_tls >= 0)
            {
                TlsNative.CloseTls(_tls);
                _tls = -1;
            }
            if (!_leaveInnerStreamOpen) _inner.Close();
        }
    }
}
#endif
