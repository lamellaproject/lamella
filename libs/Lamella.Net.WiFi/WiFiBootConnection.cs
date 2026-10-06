// Lamella.Net.WiFi -- what a stored network does when the board starts.
namespace Lamella.Net.WiFi
{

    /// <summary>What the network stored on the board does when the board starts.</summary>
    public enum WiFiBootConnection
    {
        /// <summary>Join in the background; the program starts at once and finds the link up when it is.</summary>
        Background = 0,

        /// <summary>
        /// Join before the program starts. The board waits up to 20 seconds for the link and then up to
        /// 8 seconds for an address, and starts the program whether or not they came.
        /// </summary>
        BeforeMain = 1,

        /// <summary>Do not join; the program joins by calling <see cref="WiFiAdapter.Connect()"/>.</summary>
        OnConnect = 2,
    }
}
