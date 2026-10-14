// Lamella System.Net.NetworkInformation -- System.Net.NetworkInformation.IPInterfaceProperties
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Net.NetworkInformation
{
    public abstract class IPInterfaceProperties
    {
        protected IPInterfaceProperties() { }

        public abstract UnicastIPAddressInformationCollection UnicastAddresses { get; }

        public abstract GatewayIPAddressInformationCollection GatewayAddresses { get; }

        public abstract IPv4InterfaceProperties GetIPv4Properties();
    }
}
#endif
