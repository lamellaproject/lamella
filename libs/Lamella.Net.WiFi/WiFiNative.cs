// Lamella.Net.WiFi -- the seam to the board's radio.
namespace Lamella.Net.WiFi
{
    internal static class WiFiNative
    {
        internal const int StateLength = 256;
        internal const int StateLink = 0;
        internal const int StateSecurity = 1;
        internal const int StateSource = 2;
        internal const int StateFlags = 3;
        internal const int StateSsidLength = 4;
        internal const int StateChannel = 5;
        internal const int StateRssi = 6;
        internal const int StateBssid = 8;
        internal const int StateFailure = 14;
        internal const int StateOutcome = 15;
        internal const int StateJoin = 16;
        internal const int StateDetailLength = 20;
        internal const int StateSsid = 21;
        internal const int StateDetail = 53;
        internal const int FlagRssi = 1;
        internal const int FlagChannel = 2;
        internal const int FlagBssid = 4;
        internal const int FlagFailure = 8;
        internal const int FlagOutcome = 16;

        internal const int RecordLength = 36;
        internal const int RecordSecurity = 0;
        internal const int RecordReconnection = 1;
        internal const int RecordBoot = 2;
        internal const int RecordSsidLength = 3;
        internal const int RecordSsid = 4;

        internal const int WriteFailed = 0;
        internal const int WriteWritten = 1;
        internal const int WriteUnchanged = 2;
        internal const int WriteNoRecord = 3;

        [Lamella.Runtime.RuntimeProvided]
        [Lamella.Runtime.IntendedDefault]
        internal static int Radio() { return 0; }

        [Lamella.Runtime.RuntimeProvided]
        internal static int JoinStart(byte[] ssid, byte[] secret, int security, int reconnection) { return 0; }

        [Lamella.Runtime.RuntimeProvided]
        internal static int JoinStored() { return 0; }

        [Lamella.Runtime.RuntimeProvided]
        internal static void Disconnect() { }

        [Lamella.Runtime.RuntimeProvided]
        internal static int State(byte[] report) { return 0; }

        [Lamella.Runtime.RuntimeProvided]
        internal static int RecordRead(byte[] report) { return 0; }

        [Lamella.Runtime.RuntimeProvided]
        internal static int RecordWrite(byte[] ssid, byte[] secret, int security, int reconnection) { return 0; }

        [Lamella.Runtime.RuntimeProvided]
        internal static int RecordSetBoot(int boot) { return 0; }

        [Lamella.Runtime.RuntimeProvided]
        internal static int RecordClear() { return 0; }
    }
}
