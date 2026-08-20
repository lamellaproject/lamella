// System.IO.Ports (libs/, real .NET's own assembly name) -- System.IO.Ports.SerialDataReceivedEventHandler
#if LAMELLA_SURFACE_SERIAL && LAMELLA_SURFACE_NETFX_2_0 && LAMELLA_SURFACE_THREADS
namespace System.IO.Ports
{
    /// <summary>Represents the method that will handle the <see cref="SerialPort.DataReceived"/>
    /// event of a <see cref="SerialPort"/> object.</summary>
    public delegate void SerialDataReceivedEventHandler(object sender, SerialDataReceivedEventArgs e);
}
#endif
