// Lamella System.Net.NetworkInformation -- System.Net.NetworkInformation.IPv4InterfaceProperties
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Net.NetworkInformation
{
    public abstract class IPv4InterfaceProperties
    {
        protected IPv4InterfaceProperties() { }

        public abstract bool IsDhcpEnabled { get; }
    }
}
#endif
