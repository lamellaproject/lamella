// Lamella managed corlib (from scratch). -- System.IO.Ports.SerialData
#if LAMELLA_SURFACE_SERIAL && LAMELLA_SURFACE_THREADS
namespace System.IO.Ports
{
    /// <summary>Specifies the type of character that was received on the serial port of the
    /// <see cref="SerialPort"/> object.</summary>
    public enum SerialData
    {
        /// <summary>A character was received and placed in the input buffer.</summary>
        Chars = 1,
        /// <summary>The end of file character was received and placed in the input buffer.</summary>
        Eof = 2
    }
}
#endif
