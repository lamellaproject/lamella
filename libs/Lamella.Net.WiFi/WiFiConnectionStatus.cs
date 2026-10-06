// Lamella.Net.WiFi -- how a join ended.
namespace Lamella.Net.WiFi
{

    /// <summary>How a join ended. The names and numbers are those of Windows' <c>WiFiConnectionStatus</c>.</summary>
    public enum WiFiConnectionStatus
    {
        /// <summary>The join failed for a reason none of the other values names; the detail line says what the radio reported.</summary>
        UnspecifiedFailure = 0,

        /// <summary>The link is up and carries frames. Whether the board has an address is <c>NetworkInterface</c>'s report.</summary>
        Success = 1,

        /// <summary>Access to the radio was withdrawn. No board reports it; it keeps Windows' numbering.</summary>
        AccessRevoked = 2,

        /// <summary>The network refused the secret.</summary>
        InvalidCredential = 3,

        /// <summary>The network was not found.</summary>
        NetworkNotAvailable = 4,

        /// <summary>The join did not complete in the time allowed.</summary>
        Timeout = 5,

        /// <summary>The network offers no security kind the join accepts, or the radio cannot run the kind asked for.</summary>
        UnsupportedAuthenticationProtocol = 6,
    }
}
