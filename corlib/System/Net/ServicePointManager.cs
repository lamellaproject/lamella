// Lamella managed corlib (from scratch). -- System.Net.ServicePointManager
#if LAMELLA_SURFACE_NET_TLS
using System.Net.Security;

namespace System.Net
{
    public sealed class ServicePointManager
    {
        private static RemoteCertificateValidationCallback _serverCertificateValidationCallback;

#if LAMELLA_NET_2_0
        public
#else
        internal
#endif
        static RemoteCertificateValidationCallback ServerCertificateValidationCallback
        {
            get { return _serverCertificateValidationCallback; }
            set { _serverCertificateValidationCallback = value; }
        }
    }
}
#endif
