// Lamella managed corlib (from scratch). -- System.Security.Authentication.AuthenticationException
#if LAMELLA_SURFACE_NET_TLS
namespace System.Security.Authentication
{
#if LAMELLA_NET_2_0
    public
#else
    internal
#endif
    class AuthenticationException : SystemException
    {
        public AuthenticationException() : base() { }

        public AuthenticationException(string message) : base(message) { }

        public AuthenticationException(string message, Exception innerException)
            : base(message, innerException) { }
    }
}
#endif
