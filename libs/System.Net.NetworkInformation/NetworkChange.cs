// Lamella System.Net.NetworkInformation -- the network-CHANGE event surface (threaded tiers only).
#if LAMELLA_SURFACE_THREADS && LAMELLA_SURFACE_NET_CHANGE
using System;
using System.Text;
using System.Threading;

namespace System.Net.NetworkInformation
{
    public delegate void NetworkAvailabilityChangedEventHandler(object sender, NetworkAvailabilityEventArgs e);

    public delegate void NetworkAddressChangedEventHandler(object sender, EventArgs e);

    public class NetworkAvailabilityEventArgs : EventArgs
    {
        private bool _isAvailable;
        internal NetworkAvailabilityEventArgs(bool isAvailable) { _isAvailable = isAvailable; }
        public bool IsAvailable { get { return _isAvailable; } }
    }

    public sealed class NetworkChange
    {
        private NetworkChange() { }

        private static readonly object _sync = new object();
        private static NetworkAvailabilityChangedEventHandler _availHandlers;
        private static NetworkAddressChangedEventHandler _addrHandlers;
        private static bool _started;

        private const int PollIntervalMs = 250;

        public static event NetworkAvailabilityChangedEventHandler NetworkAvailabilityChanged
        {
            add { lock (_sync) { _availHandlers = (NetworkAvailabilityChangedEventHandler)Delegate.Combine(_availHandlers, value); Subscribe(); } }
            remove { lock (_sync) { _availHandlers = (NetworkAvailabilityChangedEventHandler)Delegate.Remove(_availHandlers, value); } }
        }

        public static event NetworkAddressChangedEventHandler NetworkAddressChanged
        {
            add { lock (_sync) { _addrHandlers = (NetworkAddressChangedEventHandler)Delegate.Combine(_addrHandlers, value); Subscribe(); } }
            remove { lock (_sync) { _addrHandlers = (NetworkAddressChangedEventHandler)Delegate.Remove(_addrHandlers, value); } }
        }

        private static void Subscribe()
        {
            if (!_started)
            {
                _started = true;
                Thread dispatcher = new Thread(new ThreadStart(DispatchLoop));
                dispatcher.IsBackground = true;
                dispatcher.Start();
            }
            Monitor.Pulse(_sync);
        }

        private static void DispatchLoop()
        {
            bool lastAvail = NetworkInterface.GetIsNetworkAvailable();
            string lastSig = AddressSignature();
            while (true)
            {
                bool reBaseline = false;
                lock (_sync)
                {
                    while (_availHandlers == null && _addrHandlers == null)
                    {
                        Monitor.Wait(_sync);
                        reBaseline = true;
                    }
                }
                if (reBaseline)
                {
                    lastAvail = NetworkInterface.GetIsNetworkAvailable();
                    lastSig = AddressSignature();
                }

                Thread.Sleep(PollIntervalMs);

                bool avail = NetworkInterface.GetIsNetworkAvailable();
                if (avail != lastAvail)
                {
                    lastAvail = avail;
                    RaiseAvailability(avail);
                }

                string sig = AddressSignature();
                if (sig != lastSig)
                {
                    lastSig = sig;
                    RaiseAddress();
                }
            }
        }

        private static void RaiseAvailability(bool isAvailable)
        {
            NetworkAvailabilityChangedEventHandler handlers;
            lock (_sync) { handlers = _availHandlers; }
            if (handlers != null) handlers(null, new NetworkAvailabilityEventArgs(isAvailable));
        }

        private static void RaiseAddress()
        {
            NetworkAddressChangedEventHandler handlers;
            lock (_sync) { handlers = _addrHandlers; }
            if (handlers != null) handlers(null, EventArgs.Empty);
        }

        private static string AddressSignature()
        {
            StringBuilder sb = new StringBuilder();
            NetworkInterface[] ifaces = NetworkInterface.GetAllNetworkInterfaces();
            for (int i = 0; i < ifaces.Length; i++)
            {
                sb.Append((int)ifaces[i].OperationalStatus);
                sb.Append(':');
                sb.Append(ifaces[i].IPv4Address);
                sb.Append(';');
            }
            return sb.ToString();
        }
    }
}
#endif
