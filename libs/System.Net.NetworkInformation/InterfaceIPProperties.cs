// Lamella System.Net.NetworkInformation -- the IP configuration GetIPProperties returns (internal)
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Net.NetworkInformation
{
    internal sealed class InterfaceIPProperties : IPInterfaceProperties
    {
        private readonly UnicastIPAddressInformationCollection _unicast = new UnicastIPAddressInformationCollection();
        private readonly GatewayIPAddressInformationCollection _gateways = new GatewayIPAddressInformationCollection();
        private readonly InterfaceIPv4Properties _ipv4;

        internal InterfaceIPProperties(int address, int mask, int gateway, bool dhcp)
        {
            if (address != 0) _unicast.InternalAdd(new InterfaceUnicastAddress(Unpack(address), Unpack(mask)));
            if (gateway != 0) _gateways.InternalAdd(new InterfaceGatewayAddress(Unpack(gateway)));
            _ipv4 = new InterfaceIPv4Properties(dhcp);
        }

        public override UnicastIPAddressInformationCollection UnicastAddresses { get { return _unicast; } }

        public override GatewayIPAddressInformationCollection GatewayAddresses { get { return _gateways; } }

        public override IPv4InterfaceProperties GetIPv4Properties() { return _ipv4; }

        private static IPAddress Unpack(int packed)
        {
            byte[] octets = new byte[4];
            octets[0] = (byte)(packed >> 24);
            octets[1] = (byte)(packed >> 16);
            octets[2] = (byte)(packed >> 8);
            octets[3] = (byte)packed;
            return new IPAddress(octets);
        }
    }

    internal sealed class InterfaceUnicastAddress : UnicastIPAddressInformation
    {
        private readonly IPAddress _address;
        private readonly IPAddress _mask;

        internal InterfaceUnicastAddress(IPAddress address, IPAddress mask) { _address = address; _mask = mask; }

        public override IPAddress Address { get { return _address; } }

        public override IPAddress IPv4Mask { get { return _mask; } }
    }

    internal sealed class InterfaceGatewayAddress : GatewayIPAddressInformation
    {
        private readonly IPAddress _address;

        internal InterfaceGatewayAddress(IPAddress address) { _address = address; }

        public override IPAddress Address { get { return _address; } }
    }

    internal sealed class InterfaceIPv4Properties : IPv4InterfaceProperties
    {
        private readonly bool _dhcp;

        internal InterfaceIPv4Properties(bool dhcp) { _dhcp = dhcp; }

        public override bool IsDhcpEnabled { get { return _dhcp; } }
    }
}
#endif
