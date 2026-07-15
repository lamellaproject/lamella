// Lamella.Net.Time.Nts -- the [RuntimeProvided] bridge to the TLS + AEAD crypto seams.
using System;

namespace Lamella.Net.Time
{
    internal sealed class NtsNative
    {
        [Lamella.Runtime.RuntimeProvided] internal static int ClientConfigAlpn(int stack, int verifyMode, byte[] rootsPem, string alpn) { return -1; }
        [Lamella.Runtime.RuntimeProvided] internal static int ClientNew(int config, string hostname) { return -1; }
        [Lamella.Runtime.RuntimeProvided] internal static int Process(int tls) { return 3; }
        [Lamella.Runtime.RuntimeProvided] internal static int WantsWrite(int tls) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int WriteTls(int tls, byte[] buf, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int ReadTls(int tls, byte[] buf, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int ReadPlain(int tls, byte[] buf, int offset, int count) { return -1; }
        [Lamella.Runtime.RuntimeProvided] internal static int WritePlain(int tls, byte[] buf, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static void CloseTls(int tls) { }
        [Lamella.Runtime.RuntimeProvided] internal static int DefaultStack() { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int AlpnIs(int tls, string protocol) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int ExporterKey(int tls, string label, byte[] context, int length) { return -1; }
        [Lamella.Runtime.RuntimeProvided] internal static void DropKey(int key) { }
        [Lamella.Runtime.RuntimeProvided] internal static int SivEncrypt(int key, byte[] ad, byte[] nonce, byte[] plaintext, byte[] output) { return -1; }
        [Lamella.Runtime.RuntimeProvided] internal static int SivDecrypt(int key, byte[] ad, byte[] nonce, byte[] sealedData, byte[] output) { return -1; }
        [Lamella.Runtime.RuntimeProvided] internal static int ImportKey(byte[] key) { return -1; }
    }
}
