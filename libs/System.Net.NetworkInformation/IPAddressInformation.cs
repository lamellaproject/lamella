// Lamella System.Net.NetworkInformation -- System.Net.NetworkInformation.IPAddressInformation
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Net.NetworkInformation
{
    public abstract class IPAddressInformation
    {
        protected IPAddressInformation() { }

        public abstract IPAddress Address { get; }
    }
}
#endif
