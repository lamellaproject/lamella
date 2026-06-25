// Lamella managed corlib (from scratch). -- System.Net.Sockets enums
namespace System.Net.Sockets
{
    public enum AddressFamily
    {
        Unknown = -1,
        InterNetwork = 2,
        InterNetworkV6 = 23,
    }

    public enum SocketType
    {
        Stream = 1,
        Dgram = 2,
    }

    public enum ProtocolType
    {
        Tcp = 6,
        Udp = 17,
    }

    public enum SocketFlags
    {
        None = 0,
    }
}
