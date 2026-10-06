// Lamella.Net.WiFi -- the security kinds a join accepts.
using System;

namespace Lamella.Net.WiFi
{

    /// <summary>
    /// The kinds of security a join accepts, or the one kind in force on a network. A join may accept
    /// <see cref="Open"/> alone, or any set of the secured kinds.
    /// </summary>
    /// <remarks>
    /// When a join accepts both <see cref="Wpa2"/> and <see cref="Wpa3"/>, the kind is chosen when the
    /// join starts: WPA3 when the network names it and the radio runs it, and WPA2 otherwise. A join
    /// that names one kind uses that kind alone and is never moved to another. What the choice costs:
    /// a device that imitates the network and offers WPA2 alone is joined over WPA2. It does not learn
    /// the secret, but it does capture a handshake it can attack offline, which WPA3 exists to prevent;
    /// name <see cref="Wpa3"/> alone for a network known to offer it.
    /// </remarks>
    [Flags]
    public enum WiFiSecurity
    {
        /// <summary>No kind. A join refuses it.</summary>
        None = 0,

        /// <summary>An open network, joined with no secret. It cannot be combined with a secured kind.</summary>
        Open = 1,

        /// <summary>WPA2-Personal: a passphrase of 8 to 63 characters, or the key as 64 hexadecimal digits.</summary>
        Wpa2 = 2,

        /// <summary>WPA3-Personal: a password of 1 to 128 bytes.</summary>
        Wpa3 = 4,
    }
}
