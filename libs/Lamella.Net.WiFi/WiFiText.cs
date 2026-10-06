// Lamella.Net.WiFi -- the words the Wi-Fi types print.
using System.Text;

namespace Lamella.Net.WiFi
{
    internal static class WiFiText
    {
        internal static string Status(WiFiConnectionStatus status)
        {
            switch (status)
            {
                case WiFiConnectionStatus.UnspecifiedFailure: return "UnspecifiedFailure";
                case WiFiConnectionStatus.Success: return "Success";
                case WiFiConnectionStatus.AccessRevoked: return "AccessRevoked";
                case WiFiConnectionStatus.InvalidCredential: return "InvalidCredential";
                case WiFiConnectionStatus.NetworkNotAvailable: return "NetworkNotAvailable";
                case WiFiConnectionStatus.Timeout: return "Timeout";
                case WiFiConnectionStatus.UnsupportedAuthenticationProtocol: return "UnsupportedAuthenticationProtocol";
                default: return ((int)status).ToString();
            }
        }

        internal static string Link(WiFiLinkState link)
        {
            switch (link)
            {
                case WiFiLinkState.Disconnected: return "Disconnected";
                case WiFiLinkState.Connecting: return "Connecting";
                default: return "Connected";
            }
        }

        internal static string Kinds(WiFiSecurity security)
        {
            int bits = (int)security;
            if (bits == 0)
            {
                return "None";
            }
            string text = null;
            if ((bits & (int)WiFiSecurity.Open) != 0)
            {
                text = "Open";
            }
            if ((bits & (int)WiFiSecurity.Wpa2) != 0)
            {
                text = text == null ? "Wpa2" : text + ", Wpa2";
            }
            if ((bits & (int)WiFiSecurity.Wpa3) != 0)
            {
                text = text == null ? "Wpa3" : text + ", Wpa3";
            }
            return text;
        }

        internal static string ByteCount(string ssid)
        {
            return ssid == null ? "0" : Encoding.UTF8.GetBytes(ssid).Length.ToString();
        }
    }
}
