// Lamella managed corlib (from scratch). -- System.Security.Cryptography.X509Certificates.X509Certificate2
#if LAMELLA_SURFACE_NET_TLS && LAMELLA_NET_2_0
namespace System.Security.Cryptography.X509Certificates
{
    public class X509Certificate2 : X509Certificate
    {
        private string _password;

        private byte[] _privateKeyPem;

        public X509Certificate2(byte[] rawData) : base(rawData) { _password = ""; }

        public X509Certificate2(byte[] rawData, string password) : base(rawData) { _password = password; }

        public byte[] RawData { get { return GetRawCertData(); } }

        internal byte[] GetIdentityBytes() { return GetRawCertData(); }

        internal string GetIdentityPassword() { return _password; }

        internal byte[] GetClientKeyPem() { return _privateKeyPem; }

#if LAMELLA_SURFACE_NET_5_0
        private const string NoCertificate =
            "The certificate contents do not contain a PEM with a CERTIFICATE label, or the content is malformed.";
        private const string NoKey =
            "The key contents do not contain a PEM, the content is malformed, or the key does not match the certificate.";
        private const string UnreadableKey = "ASN1 corrupted data.";
        private const string WrongKeyAlgorithm = "Key is not a valid public or private key.";

        /// <summary>
        /// Creates a certificate with its private key from RFC 7468 PEM text: the first <c>CERTIFICATE</c>
        /// field of <paramref name="certPem"/>, and the first field of <paramref name="keyPem"/> that holds a
        /// key of the certificate's algorithm -- <c>PRIVATE KEY</c> (PKCS#8), or <c>RSA PRIVATE KEY</c>
        /// (PKCS#1) for an RSA certificate, or <c>EC PRIVATE KEY</c> (SEC1) for an EC one. Text outside the
        /// fields is ignored. A TLS client presents the result through
        /// <see cref="System.Net.Security.SslStream.AuthenticateAsClient(string, X509CertificateCollection, System.Security.Authentication.SslProtocols, bool)"/>.
        /// </summary>
        /// <param name="certPem">The text of the PEM-encoded certificate.</param>
        /// <param name="keyPem">The text of the PEM-encoded private key. An encrypted key is not read.</param>
        /// <returns>The certificate, carrying its private key.</returns>
        /// <exception cref="CryptographicException">
        /// <paramref name="certPem"/> holds no well-formed certificate; or <paramref name="keyPem"/> holds no
        /// well-formed key of the certificate's algorithm, or one that is not the certificate's; or the
        /// certificate's public key has an algorithm other than RSA, EC or DSA.
        /// </exception>
        public static X509Certificate2 CreateFromPem(ReadOnlySpan<char> certPem, ReadOnlySpan<char> keyPem)
        {
            PemField certificate = PemField.Find(new String(certPem.ToArray()), new string[] { "CERTIFICATE" });
            byte[] algorithm = (object)certificate == null ? null : PemField.CertificateKeyAlgorithm(certificate.Data);
            if ((object)algorithm == null) throw new CryptographicException(NoCertificate);

            byte[] rsaAlgorithm = { 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01 };
            byte[] ecAlgorithm = { 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01 };
            byte[] dsaAlgorithm = { 0x2A, 0x86, 0x48, 0xCE, 0x38, 0x04, 0x01 };
            string[] keyLabels;
            if (SameBytes(algorithm, rsaAlgorithm))
            {
                keyLabels = new string[] { "PRIVATE KEY", "RSA PRIVATE KEY" };
            }
            else if (SameBytes(algorithm, ecAlgorithm))
            {
                keyLabels = new string[] { "PRIVATE KEY", "EC PRIVATE KEY" };
            }
            else if (SameBytes(algorithm, dsaAlgorithm))
            {
                keyLabels = new string[] { "PRIVATE KEY" };
            }
            else
            {
                throw new CryptographicException("'" + PemField.OidText(algorithm) + "' is not a known key algorithm.");
            }

            string keyText = new String(keyPem.ToArray());
            PemField key = PemField.Find(keyText, keyLabels);
            if ((object)key == null) throw new CryptographicException(NoKey);
            if (key.Label == "PRIVATE KEY")
            {
                byte[] keyAlgorithm = PemField.Pkcs8KeyAlgorithm(key.Data);
                if ((object)keyAlgorithm == null) throw new CryptographicException(UnreadableKey);
                if (!SameBytes(keyAlgorithm, algorithm)) throw new CryptographicException(WrongKeyAlgorithm);
            }

            byte[] keyField = new byte[key.End - key.Start];
            for (int i = 0; i < keyField.Length; i++)
            {
                keyField[i] = (byte)keyText[key.Start + i];
            }
            switch (System.Net.Security.TlsNative.CheckIdentity(certificate.Data, keyField))
            {
                case IdentityCertificateUnreadable:
                    throw new CryptographicException(NoCertificate);
                case IdentityKeyUnreadable:
                    throw new CryptographicException(UnreadableKey);
                case IdentityKeyMismatch:
                    throw new CryptographicException(NoKey);
            }

            X509Certificate2 created = new X509Certificate2(certificate.Data);
            created._privateKeyPem = keyField;
            return created;
        }

        private const int IdentityCertificateUnreadable = -3;
        private const int IdentityKeyUnreadable = -4;
        private const int IdentityKeyMismatch = -5;

        private static bool SameBytes(byte[] a, byte[] b)
        {
            if (a.Length != b.Length) return false;
            for (int i = 0; i < a.Length; i++)
            {
                if (a[i] != b[i]) return false;
            }
            return true;
        }
#endif
    }
}
#endif
