// Lamella managed corlib (from scratch). -- System.Security.Cryptography.X509Certificates.X509Certificate2
#if LAMELLA_SURFACE_NET_TLS && LAMELLA_NET_2_0
namespace System.Security.Cryptography.X509Certificates
{
    public class X509Certificate2 : X509Certificate
    {
        private string _password;

        public X509Certificate2(byte[] rawData) : base(rawData) { _password = ""; }

        public X509Certificate2(byte[] rawData, string password) : base(rawData) { _password = password; }

        public byte[] RawData { get { return GetRawCertData(); } }

        internal byte[] GetIdentityBytes() { return GetRawCertData(); }

        internal string GetIdentityPassword() { return _password; }
    }
}
#endif
