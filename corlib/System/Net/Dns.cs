// Lamella managed corlib (from scratch). -- System.Net.Dns
#if LAMELLA_SURFACE_NET
using System.Net.Sockets;

namespace System.Net
{
    public class Dns
    {
        [Lamella.Runtime.RuntimeProvided] private static int ResolveHost(string host, byte[] buffer, int[] lengths) { return 0; }

        private const int MaxAddresses = 8;

        public static IPAddress[] GetHostAddresses(string hostNameOrAddress)
        {
            if ((object)hostNameOrAddress == null) throw new ArgumentNullException("hostNameOrAddress");
            IPAddress literal;
            if (IPAddress.TryParse(hostNameOrAddress, out literal))
            {
                IPAddress[] single = new IPAddress[1];
                single[0] = literal;
                return single;
            }
            byte[] buffer = new byte[16 * MaxAddresses];
            int[] lengths = new int[MaxAddresses];
            int count = ResolveHost(hostNameOrAddress, buffer, lengths);
            if (count <= 0) throw new SocketException();
            IPAddress[] result = new IPAddress[count];
            for (int i = 0; i < count; i++)
            {
                byte[] octets = new byte[lengths[i]];
                for (int j = 0; j < lengths[i]; j++) octets[j] = buffer[i * 16 + j];
                result[i] = new IPAddress(octets);
            }
            return result;
        }

        public static IPHostEntry GetHostEntry(string hostNameOrAddress)
        {
            IPAddress[] addresses = GetHostAddresses(hostNameOrAddress);
            IPHostEntry entry = new IPHostEntry();
            entry.HostName = hostNameOrAddress;
            entry.AddressList = addresses;
            entry.Aliases = new string[0];
            return entry;
        }

        public static IPHostEntry GetHostByName(string hostName) { return GetHostEntry(hostName); }
        public static IPHostEntry Resolve(string hostName) { return GetHostEntry(hostName); }
    }
}
#endif
