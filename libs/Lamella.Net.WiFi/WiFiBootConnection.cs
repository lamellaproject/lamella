// Lamella.Net.WiFi -- what a stored network does when the board starts.
namespace Lamella.Net.WiFi
{
    /// <summary>
    /// What the network stored on the board does when the board starts. Neither value holds the
    /// program's start for the network: a program that needs it waits with
    /// <see cref="WiFiAdapter.WaitForAddress(System.TimeSpan)"/>.
    /// </summary>
    public enum WiFiBootConnection
    {
        /// <summary>Join in the background; the program starts at once and finds the link up when it is.</summary>
        Background = 0,

        /// <summary>Do not join; the program joins by calling <see cref="WiFiAdapter.Connect()"/>.</summary>
        OnConnect = 2,
    }
}
