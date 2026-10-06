// Lamella.Net.WiFi -- the radio's state at one moment.
namespace Lamella.Net.WiFi
{

    /// <summary>
    /// The radio's state at the moment it was read: the link, the network held, the kind of security
    /// in force, where the credential came from, and the last join that failed. Read
    /// <see cref="WiFiAdapter.ConnectionState"/> again for a later moment.
    /// </summary>
    public sealed class WiFiConnectionState
    {
        private readonly WiFiLinkState _link;
        private readonly string _ssid;
        private readonly WiFiSecurity _security;
        private readonly WiFiCredentialSource _source;
        private readonly int? _rssi;
        private readonly int? _channel;
        private readonly string _bssid;
        private readonly WiFiConnectionResult _lastFailure;

        internal WiFiConnectionState(
            WiFiLinkState link,
            string ssid,
            WiFiSecurity security,
            WiFiCredentialSource source,
            int? rssi,
            int? channel,
            string bssid,
            WiFiConnectionResult lastFailure)
        {
            _link = link;
            _ssid = ssid;
            _security = security;
            _source = source;
            _rssi = rssi;
            _channel = channel;
            _bssid = bssid;
            _lastFailure = lastFailure;
        }

        /// <summary>The link.</summary>
        public WiFiLinkState Link { get { return _link; } }

        /// <summary>The name of the network held or being joined, decoded as UTF-8; null when none is.</summary>
        public string Ssid { get { return _ssid; } }

        /// <summary>The one kind of security in force once the join has chosen it; <see cref="WiFiSecurity.None"/> before that.</summary>
        public WiFiSecurity Security { get { return _security; } }

        /// <summary>Where the credential of the network held came from.</summary>
        public WiFiCredentialSource Source { get { return _source; } }

        /// <summary>The signal strength in dBm, measured when the network was joined, where the radio reported it; null otherwise.</summary>
        public int? Rssi { get { return _rssi; } }

        /// <summary>The channel, where the radio reported it; null otherwise.</summary>
        public int? Channel { get { return _channel; } }

        /// <summary>The access point's address as six pairs of hexadecimal digits, <c>aa:bb:cc:dd:ee:ff</c>, once the link has been up; null otherwise.</summary>
        public string Bssid { get { return _bssid; } }


        /// <summary>
        /// How the last join that failed ended, with its detail line, until a join succeeds; null when
        /// none has failed since.
        /// </summary>
        public WiFiConnectionResult LastFailure { get { return _lastFailure; } }

        /// <summary>The link, the length of the network's name and the kind in force. Never the name.</summary>
        public override string ToString()
        {
            if (_link == WiFiLinkState.Disconnected)
            {
                return _lastFailure == null ? "Disconnected" : "Disconnected; last failure " + WiFiText.Status(_lastFailure.ConnectionStatus);
            }
            string text = WiFiText.Link(_link) + ", a network named in " + WiFiText.ByteCount(_ssid) + " bytes";
            if (_security != WiFiSecurity.None)
            {
                text = text + " (" + WiFiText.Kinds(_security) + ")";
            }
            return text;
        }
    }
}
