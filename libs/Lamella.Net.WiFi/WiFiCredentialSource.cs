// Lamella.Net.WiFi -- where the credential of the network held came from.
namespace Lamella.Net.WiFi
{

    /// <summary>Where the credential of the network held came from.</summary>
    public enum WiFiCredentialSource
    {
        /// <summary>No network is held.</summary>
        None = 0,

        /// <summary>A program named the network in a call to <see cref="WiFiAdapter.Connect(string, System.ReadOnlySpan{byte}, WiFiSecurity, WiFiPersistence)"/>.</summary>
        Program = 1,

        /// <summary>The network stored on the board.</summary>
        StoredRecord = 2,

        /// <summary>The network named when the firmware was built, which only a development build carries.</summary>
        BuildTime = 3,
    }
}
