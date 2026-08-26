// Microsoft.SPOT.Hardware (a .NET Micro Framework compatibility assembly) -- NativeEventDispatcher.
namespace Microsoft.SPOT.Hardware
{
    /// <summary>Handles an event raised by a native driver.</summary>
    /// <param name="data1">The first datum the driver reported.</param>
    /// <param name="data2">The second datum the driver reported.</param>
    /// <param name="time">When the driver reported it.</param>
    public delegate void NativeEventHandler(uint data1, uint data2, System.DateTime time);

    /// <summary>Dispatches events raised by a native driver to managed handlers.</summary>
    /// <remarks>
    /// IMPORTANT: this build delivers no hardware interrupts to managed code, so a handler added to
    /// <see cref="OnInterrupt"/> is never invoked. Poll the port instead, or wait on
    /// System.Device.Gpio's WaitForEvent, until the interrupt seam lands.
    /// </remarks>
    public class NativeEventDispatcher : System.IDisposable
    {
        /// <summary>The handlers registered for this dispatcher.</summary>
        protected NativeEventHandler m_callbacks;
        /// <summary>Whether this dispatcher has been disposed.</summary>
        protected bool m_disposed;

        /// <summary>Initializes a dispatcher with no registered handlers.</summary>
        protected NativeEventDispatcher()
        {
            m_callbacks = null;
            m_disposed = false;
        }

        /// <summary>Enables the interrupt this dispatcher reports.</summary>
        public virtual void EnableInterrupt()
        {
        }

        /// <summary>Disables the interrupt this dispatcher reports.</summary>
        public virtual void DisableInterrupt()
        {
        }

        /// <summary>Raised when the underlying driver reports an event.</summary>
        /// <remarks>See the type remarks: nothing raises this in the current build.</remarks>
        public event NativeEventHandler OnInterrupt
        {
            add
            {
                if (m_disposed)
                {
                    throw new System.ObjectDisposedException("the dispatcher is disposed");
                }
                m_callbacks = (NativeEventHandler)System.Delegate.Combine(m_callbacks, value);
            }
            remove
            {
                if (m_disposed)
                {
                    throw new System.ObjectDisposedException("the dispatcher is disposed");
                }
                m_callbacks = (NativeEventHandler)System.Delegate.Remove(m_callbacks, value);
            }
        }

        /// <summary>Releases the resources this dispatcher holds.</summary>
        public virtual void Dispose()
        {
            if (!m_disposed)
            {
                Dispose(true);
                System.GC.SuppressFinalize(this);
                m_disposed = true;
            }
        }

        /// <summary>Releases the resources this dispatcher holds.</summary>
        /// <param name="disposing">True when called from <see cref="Dispose()"/>.</param>
        protected virtual void Dispose(bool disposing)
        {
        }
    }
}
