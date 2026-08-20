// System.IO.Ports (libs/, real .NET's own assembly name) -- System.IO.Ports.SerialDataReceivedEventArgs
#if LAMELLA_SURFACE_SERIAL && LAMELLA_SURFACE_NETFX_2_0 && LAMELLA_SURFACE_THREADS
namespace System.IO.Ports
{
    /// <summary>Provides data for the <see cref="SerialPort.DataReceived"/> event.</summary>
    public class SerialDataReceivedEventArgs : EventArgs
    {
        private SerialData _eventType;

        internal SerialDataReceivedEventArgs(SerialData eventType)
        {
            _eventType = eventType;
        }

        /// <summary>The event type.</summary>
        public SerialData EventType
        {
            get { return _eventType; }
        }
    }
}
#endif
