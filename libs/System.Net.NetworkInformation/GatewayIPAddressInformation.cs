// Lamella System.Net.NetworkInformation -- System.Net.NetworkInformation.GatewayIPAddressInformation
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Net.NetworkInformation
{
    public abstract class GatewayIPAddressInformation
    {
        protected GatewayIPAddressInformation() { }

        public abstract IPAddress Address { get; }
    }
}
#endif
