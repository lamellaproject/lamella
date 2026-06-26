// Lamella managed corlib (from scratch). -- System.Net.Security.RemoteCertificateValidationCallback
#if LAMELLA_SURFACE_NET_TLS
using System.Security.Cryptography.X509Certificates;

namespace System.Net.Security
{
#if LAMELLA_NET_2_0
    public
#else
    internal
#endif
    delegate bool RemoteCertificateValidationCallback(
        object sender,
        X509Certificate certificate,
        X509Chain chain,
        SslPolicyErrors sslPolicyErrors);
}
#endif
