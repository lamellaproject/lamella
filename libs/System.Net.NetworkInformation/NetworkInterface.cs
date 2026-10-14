// Lamella System.Net.NetworkInformation -- the network-state poll surface, a separate assembly.
namespace System.Net.NetworkInformation
{
    public sealed class NetworkInterface
    {
        [Lamella.Runtime.RuntimeProvided] private static int NetworkAvailable() { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int InterfaceCount() { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int OperStatus(int index) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int IfaceType(int index) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int IPv4(int index) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int Ipv4Mask(int index) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int Ipv4Gateway(int index) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int IfaceFlags(int index) { return 0; }

        [Lamella.Runtime.RuntimeProvided]
        [Lamella.Runtime.IntendedDefault]
        private static int ChangeCount(int which) { return -1; }

        internal static int Changes(int which) { return ChangeCount(which); }

        private readonly int _index;

        private NetworkInterface(int index) { _index = index; }

        public static bool GetIsNetworkAvailable()
        {
            return NetworkAvailable() != 0;
        }

        public static NetworkInterface[] GetAllNetworkInterfaces()
        {
            int count = InterfaceCount();
            if (count < 0) count = 0;
            NetworkInterface[] result = new NetworkInterface[count];
            for (int i = 0; i < count; i++) result[i] = new NetworkInterface(i);
            return result;
        }

        public OperationalStatus OperationalStatus
        {
            get { return (OperationalStatus)OperStatus(_index); }
        }

        public NetworkInterfaceType NetworkInterfaceType
        {
            get { return (NetworkInterfaceType)IfaceType(_index); }
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public IPInterfaceProperties GetIPProperties()
        {
            return new InterfaceIPProperties(IPv4(_index), Ipv4Mask(_index), Ipv4Gateway(_index),
                (IfaceFlags(_index) & 1) != 0);
        }
#endif

        internal int PackedIPv4
        {
            get { return IPv4(_index); }
        }

        public string Name
        {
            get
            {
                string prefix;
                int kind = IfaceType(_index);
                if (kind == 6) prefix = "eth";
                else if (kind == 71) prefix = "wlan";
                else if (kind == 24) prefix = "lo";
                else prefix = "net";
                return prefix + _index.ToString();
            }
        }
    }
}
