// Lamella.Net.WiFi -- what the radio does when the link is lost.
namespace Lamella.Net.WiFi
{

    /// <summary>What the radio does when the link to a network it joined is lost.</summary>
    public enum WiFiReconnectionKind
    {
        /// <summary>
        /// Join the network again, resting between attempts, until the program disconnects. Each loss
        /// and each return raises <c>NetworkChange</c>'s events.
        /// </summary>
        Automatic = 0,

        /// <summary>Let the network go when the link is lost; the program joins again when it chooses.</summary>
        Manual = 1,
    }
}
