// Lamella managed corlib (from scratch). -- System.Security.Cryptography.X509Certificates.X509Certificate
#if LAMELLA_SURFACE_NET_TLS
namespace System.Security.Cryptography.X509Certificates
{
    public class X509Certificate
    {
        internal byte[] _certData;

        public X509Certificate() { }

        public X509Certificate(byte[] data) { _certData = data; }

        public virtual byte[] GetRawCertData() { return _certData; }
    }
}
#endif
