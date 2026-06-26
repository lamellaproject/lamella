// Lamella managed corlib (from scratch). -- System.Net.Security.SslPolicyErrors
#if LAMELLA_SURFACE_NET_TLS
namespace System.Net.Security
{
#if LAMELLA_NET_2_0
    public
#else
    internal
#endif
    enum SslPolicyErrors
    {
        None = 0,
        RemoteCertificateNotAvailable = 1,
        RemoteCertificateNameMismatch = 2,
        RemoteCertificateChainErrors = 4,
    }
}
#endif
