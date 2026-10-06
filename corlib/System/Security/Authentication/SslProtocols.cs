// Lamella managed corlib (from scratch). -- System.Security.Authentication.SslProtocols
#if LAMELLA_SURFACE_NET_TLS && LAMELLA_NET_2_0
namespace System.Security.Authentication
{

    /// <summary>The versions of the SSL and TLS protocols a <see cref="System.Net.Security.SslStream"/> may use.</summary>
    [Flags]
    public enum SslProtocols
    {
        /// <summary>No version named: the engine's own choice.</summary>
        None = 0,

        /// <summary>SSL 2.0.</summary>
        Ssl2 = 12,

        /// <summary>SSL 3.0.</summary>
        Ssl3 = 48,

        /// <summary>TLS 1.0.</summary>
        Tls = 192,

        /// <summary>SSL 3.0 or TLS 1.0.</summary>
        Default = Ssl3 | Tls,
#if LAMELLA_SURFACE_NETFX_4_5

        /// <summary>TLS 1.1.</summary>
        Tls11 = 768,

        /// <summary>TLS 1.2.</summary>
        Tls12 = 3072,
#endif
    }
}
#endif
