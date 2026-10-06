// Lamella.Net.WiFi -- the link, as a snapshot reports it.
namespace Lamella.Net.WiFi
{

    /// <summary>The radio's link to a network.</summary>
    public enum WiFiLinkState
    {
        /// <summary>No network is held.</summary>
        Disconnected = 0,

        /// <summary>A join, or a join again after the link was lost, is in progress.</summary>
        Connecting = 1,

        /// <summary>The link carries frames.</summary>
        Connected = 2,
    }
}
