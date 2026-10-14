// Lamella System.Net.NetworkInformation -- System.Net.NetworkInformation.UnicastIPAddressInformation
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Net.NetworkInformation
{
    public abstract class UnicastIPAddressInformation : IPAddressInformation
    {
        protected UnicastIPAddressInformation() { }

        public abstract IPAddress IPv4Mask { get; }
    }
}
#endif
