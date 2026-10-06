// Lamella.Net.WiFi -- whether a joined network is stored on the board.
namespace Lamella.Net.WiFi
{
    /// <summary>Whether a network a program joins is also stored on the board for later joins.</summary>
    public enum WiFiPersistence
    {
        /// <summary>Join the network for now; the board does not store it.</summary>
        Temporary = 0,

        /// <summary>
        /// Join the network, and once the join succeeds, store it on the board in place of any network
        /// stored before. The name and secret it replaces are erased. A failed join stores nothing.
        /// </summary>
        Persistent = 1,
    }
}
