// Lamella.Net.WiFi -- the network stored on the board, without its secret.
namespace Lamella.Net.WiFi
{
    /// <summary>
    /// The network stored on the board, as a program may read it. The secret is never read back: the
    /// board holds it for its own joins alone.
    /// </summary>
    public sealed class WiFiStoredNetwork
    {
        private readonly string _ssid;
        private readonly WiFiSecurity _security;
        private readonly WiFiReconnectionKind _reconnection;
        private readonly WiFiBootConnection _boot;

        internal WiFiStoredNetwork(string ssid, WiFiSecurity security, WiFiReconnectionKind reconnection, WiFiBootConnection boot)
        {
            _ssid = ssid;
            _security = security;
            _reconnection = reconnection;
            _boot = boot;
        }

        /// <summary>The network's name, decoded as UTF-8.</summary>
        public string Ssid { get { return _ssid; } }

        /// <summary>The kinds of security a join of this network accepts.</summary>
        public WiFiSecurity Security { get { return _security; } }

        /// <summary>What the radio does when the link to this network is lost.</summary>
        public WiFiReconnectionKind Reconnection { get { return _reconnection; } }

        /// <summary>What this network does when the board starts.</summary>
        public WiFiBootConnection BootConnection { get { return _boot; } }

        /// <summary>The length of the network's name and its settings. Never the name.</summary>
        public override string ToString()
        {
            return "A network named in " + WiFiText.ByteCount(_ssid) + " bytes (" + WiFiText.Kinds(_security) + ")";
        }
    }
}
