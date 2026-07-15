// Lamella managed corlib (from scratch). -- System.IO.Ports.NativeSerial (the serial-port seam)
#if LAMELLA_SURFACE_SERIAL
namespace System.IO.Ports
{
    internal static class NativeSerial
    {
        internal const int ErrNotFound = -2;
        internal const int ErrAccessDenied = -3;
        internal const int ErrTimeout = -4;
        internal const int ErrIo = -5;

        internal static void Throw(int code, string portName)
        {
            if (code == ErrNotFound)
                throw new IOException("The port '" + portName + "' does not exist.");
            if (code == ErrAccessDenied)
                throw new UnauthorizedAccessException("Access to the port '" + portName + "' is denied.");
            if (code == ErrTimeout)
                throw new TimeoutException("The write timed out.");
            throw new IOException("An I/O error occurred while accessing the port '" + portName + "'.");
        }

        [Lamella.Runtime.RuntimeProvided] internal static int Open(string portName, int baudRate, int parity, int dataBits, int stopBits, int handshake) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Read(int handle, byte[] buffer, int offset, int count, int timeoutMs) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Write(int handle, byte[] buffer, int offset, int count, int timeoutMs) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int BytesToRead(int handle) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int BytesToWrite(int handle) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Flush(int handle) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int DiscardIn(int handle) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int DiscardOut(int handle) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static void Close(int handle) { }
    }
}
#endif
