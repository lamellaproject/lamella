// Lamella managed corlib (from scratch). -- System.Net.Security.TlsNative
#if LAMELLA_SURFACE_NET_TLS
namespace System.Net.Security
{
    internal sealed class TlsNative
    {
        [Lamella.Runtime.RuntimeProvided] internal static int ClientConfig(int stack, int verifyMode, byte[] rootsPem) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int ServerConfig(int stack, byte[] pfx, string password) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int ClientNew(int config, string hostname) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int ServerNew(int config) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Process(int tls) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int WantsWrite(int tls) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int WriteTls(int tls, byte[] buf, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int ReadTls(int tls, byte[] buf, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int ReadPlain(int tls, byte[] buf, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int WritePlain(int tls, byte[] buf, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int PeerCert(int tls, byte[] buf) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static void CloseTls(int tls) { }
        [Lamella.Runtime.RuntimeProvided] internal static int DefaultStack() { return 0; }
    }
}
#endif
