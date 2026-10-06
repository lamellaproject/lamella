// Lamella.Net.WiFi -- the board's Wi-Fi radio.
using System;
using System.IO;
using System.Net.NetworkInformation;
using System.Text;
using System.Threading;

namespace Lamella.Net.WiFi
{

    /// <summary>
    /// The board's Wi-Fi radio: joins a network the program names or the one stored on the board,
    /// reports the link, stores a network for later joins, and erases it.
    /// </summary>
    /// <remarks>
    /// <para>
    /// The network stored on the board is the default and a network the program names is the override,
    /// and they never mix: a join that names a network joins exactly that network or fails, and never
    /// falls back to the stored one.
    /// </para>
    /// <para>
    /// A join's outcome is a value and a mistake is an exception. Every way a join can fail comes back in
    /// <see cref="WiFiConnectionResult.ConnectionStatus"/>; a call that could not succeed whatever the
    /// network does throws <see cref="ArgumentException"/>, <see cref="InvalidOperationException"/> or,
    /// where no radio is running, <see cref="PlatformNotSupportedException"/>.
    /// </para>
    /// <para>
    /// A secret is never kept by these classes: it is handed to the radio for the join, and to the
    /// board's storage when the network is stored, and the copies made on the way are cleared. Nothing
    /// reads a stored secret back. A network's name is printed by no <c>ToString</c> here; its length is.
    /// </para>
    /// <para>
    /// The addresses the board holds on the network are <see cref="NetworkInterface"/>'s report, and
    /// <see cref="NetworkChange"/>'s events announce a link that comes up or goes down and an address
    /// that arrives or changes.
    /// </para>
    /// </remarks>
    public sealed class WiFiAdapter
    {
        private const int DefaultTimeoutMilliseconds = 20000;
        private const int PollMilliseconds = 20;
        private const int AddressPollMilliseconds = 50;
        private const int SsidMax = 32;
        private const int Wpa2PassphraseMin = 8;
        private const int Wpa2PassphraseMax = 63;
        private const int Wpa2KeyDigits = 64;
        private const int Wpa3PasswordMax = 128;

        private const string NoStoredNetwork =
            "No Wi-Fi network is stored on this board. Pass a network to Connect, with WiFiPersistence.Persistent to store it for later joins.";

        private static WiFiAdapter _default;

        private WiFiAdapter()
        {
        }

        /// <summary>
        /// Whether a Wi-Fi radio is running: false on a board without one, on a board whose radio did
        /// not start, and on a platform these classes have no radio seam on.
        /// </summary>
        public static bool IsSupported
        {
            get { return (WiFiNative.Radio() & 1) != 0; }
        }

        /// <summary>The board's radio.</summary>
        /// <exception cref="PlatformNotSupportedException">No radio is running (<see cref="IsSupported"/> is false).</exception>
        public static WiFiAdapter Default
        {
            get
            {
                Require();
                if (_default == null)
                {
                    _default = new WiFiAdapter();
                }
                return _default;
            }
        }

        /// <summary>
        /// Joins the network named <paramref name="ssid"/>, waiting up to 20 seconds, and re-joins it
        /// automatically when the link is lost.
        /// </summary>
        /// <param name="ssid">The network's name: 1 to 32 bytes once encoded as UTF-8.</param>
        /// <param name="secret">The passphrase or password, as bytes; empty for an open network. The caller may clear it once the call returns.</param>
        /// <param name="security">The kinds of security the join accepts.</param>
        /// <param name="persistence">Whether the network is stored on the board once the join succeeds.</param>
        /// <returns>How the join ended.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="ssid"/> is null.</exception>
        /// <exception cref="ArgumentException">The name, the secret or the kinds are outside their bounds; see <see cref="WiFiSecurity"/>.</exception>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        /// <exception cref="IOException">The join succeeded and <see cref="WiFiPersistence.Persistent"/> was asked for, but the board could not store the network.</exception>
        public WiFiConnectionResult Connect(string ssid, ReadOnlySpan<byte> secret, WiFiSecurity security, WiFiPersistence persistence)
        {
            return Connect(ssid, secret, security, persistence, WiFiReconnectionKind.Automatic, DefaultTimeout());
        }

        /// <summary>Joins the network named <paramref name="ssid"/>, waiting up to <paramref name="timeout"/>.</summary>
        /// <param name="ssid">The network's name: 1 to 32 bytes once encoded as UTF-8.</param>
        /// <param name="secret">The passphrase or password, as bytes; empty for an open network.</param>
        /// <param name="security">The kinds of security the join accepts.</param>
        /// <param name="persistence">Whether the network is stored on the board once the join succeeds.</param>
        /// <param name="reconnection">What the radio does when the link is lost; stored with the network when it is stored.</param>
        /// <param name="timeout">How long to wait for the link: from zero to <see cref="int.MaxValue"/> milliseconds, or -1 millisecond to wait without a bound. A join still going when it passes is ended, and reported with the failure it last met, or as <see cref="WiFiConnectionStatus.Timeout"/>.</param>
        /// <returns>How the join ended.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="ssid"/> is null.</exception>
        /// <exception cref="ArgumentException">The name, the secret or the kinds are outside their bounds.</exception>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="persistence"/>, <paramref name="reconnection"/> or <paramref name="timeout"/> is outside its range.</exception>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        /// <exception cref="IOException">The join succeeded and the network was to be stored, but the board could not store it.</exception>
        public WiFiConnectionResult Connect(
            string ssid,
            ReadOnlySpan<byte> secret,
            WiFiSecurity security,
            WiFiPersistence persistence,
            WiFiReconnectionKind reconnection,
            TimeSpan timeout)
        {
            Require();
            if (ssid == null)
            {
                throw new ArgumentNullException("ssid");
            }
            return Join(Encoding.UTF8.GetBytes(ssid), secret, security, persistence, reconnection, timeout);
        }

        /// <summary>
        /// Joins the network whose name is the bytes <paramref name="ssid"/>, for a name that is not
        /// text, waiting up to <paramref name="timeout"/>.
        /// </summary>
        /// <param name="ssid">The network's name: 1 to 32 bytes.</param>
        /// <param name="secret">The passphrase or password, as bytes; empty for an open network.</param>
        /// <param name="security">The kinds of security the join accepts.</param>
        /// <param name="persistence">Whether the network is stored on the board once the join succeeds.</param>
        /// <param name="reconnection">What the radio does when the link is lost.</param>
        /// <param name="timeout">How long to wait for the link, as for the overload that takes a name as text.</param>
        /// <returns>How the join ended.</returns>
        /// <exception cref="ArgumentException">The name, the secret or the kinds are outside their bounds.</exception>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="persistence"/>, <paramref name="reconnection"/> or <paramref name="timeout"/> is outside its range.</exception>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        /// <exception cref="IOException">The join succeeded and the network was to be stored, but the board could not store it.</exception>
        public WiFiConnectionResult Connect(
            ReadOnlySpan<byte> ssid,
            ReadOnlySpan<byte> secret,
            WiFiSecurity security,
            WiFiPersistence persistence,
            WiFiReconnectionKind reconnection,
            TimeSpan timeout)
        {
            Require();
            return Join(ssid.ToArray(), secret, security, persistence, reconnection, timeout);
        }

        /// <summary>Joins the network stored on the board, waiting up to 20 seconds.</summary>
        /// <returns>How the join ended.</returns>
        /// <exception cref="InvalidOperationException">No network is stored on the board.</exception>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        public WiFiConnectionResult Connect()
        {
            return Connect(DefaultTimeout());
        }

        /// <summary>Joins the network stored on the board, waiting up to <paramref name="timeout"/>.</summary>
        /// <param name="timeout">How long to wait for the link, as for the overloads that name a network.</param>
        /// <returns>How the join ended.</returns>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="timeout"/> is outside its range.</exception>
        /// <exception cref="InvalidOperationException">No network is stored on the board.</exception>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        public WiFiConnectionResult Connect(TimeSpan timeout)
        {
            Require();
            int limit = Milliseconds(timeout, "timeout");
            int join = WiFiNative.JoinStored();
            if (join <= 0)
            {
                throw new InvalidOperationException(NoStoredNetwork);
            }
            return Follow(join, limit);
        }

        /// <summary>
        /// Drops the network held and ends a join in progress. The radio forgets the secret it held for
        /// it; a network stored on the board stays stored.
        /// </summary>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        public void Disconnect()
        {
            Require();
            WiFiNative.Disconnect();
        }

        /// <summary>The radio's state now.</summary>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        public WiFiConnectionState ConnectionState
        {
            get
            {
                Require();
                byte[] report = new byte[WiFiNative.StateLength];
                WiFiNative.State(report);
                int flags = report[WiFiNative.StateFlags];
                int ssidLength = report[WiFiNative.StateSsidLength];
                string ssid = null;
                if (ssidLength != 0)
                {
                    ssid = Encoding.UTF8.GetString(report, WiFiNative.StateSsid, ssidLength);
                }
                int? rssi = null;
                if ((flags & WiFiNative.FlagRssi) != 0)
                {
                    rssi = (short)(report[WiFiNative.StateRssi] | (report[WiFiNative.StateRssi + 1] << 8));
                }
                int? channel = null;
                if ((flags & WiFiNative.FlagChannel) != 0)
                {
                    channel = report[WiFiNative.StateChannel];
                }
                string bssid = null;
                if ((flags & WiFiNative.FlagBssid) != 0)
                {
                    bssid = HexPairs(report, WiFiNative.StateBssid, 6);
                }
                WiFiConnectionResult lastFailure = null;
                if ((flags & WiFiNative.FlagFailure) != 0)
                {
                    lastFailure = new WiFiConnectionResult(
                        (WiFiConnectionStatus)report[WiFiNative.StateFailure], WiFiSecurity.None, Detail(report));
                }
                return new WiFiConnectionState(
                    (WiFiLinkState)report[WiFiNative.StateLink],
                    ssid,
                    (WiFiSecurity)report[WiFiNative.StateSecurity],
                    (WiFiCredentialSource)report[WiFiNative.StateSource],
                    rssi,
                    channel,
                    bssid,
                    lastFailure);
            }
        }


        /// <summary>
        /// Waits until the board holds an address on the Wi-Fi network, as <see cref="NetworkInterface"/>
        /// reports it, or until <paramref name="timeout"/> passes. Returns at once, false, when no
        /// network is held or being joined.
        /// </summary>
        /// <param name="timeout">How long to wait: from zero to <see cref="int.MaxValue"/> milliseconds, or -1 millisecond to wait without a bound.</param>
        /// <returns>Whether the board holds an address.</returns>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="timeout"/> is outside its range.</exception>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        public bool WaitForAddress(TimeSpan timeout)
        {
            Require();
            int limit = Milliseconds(timeout, "timeout");
            int start = Environment.TickCount;
            byte[] report = new byte[WiFiNative.StateLength];
            while (true)
            {
                if (HasWirelessAddress())
                {
                    return true;
                }
                WiFiNative.State(report);
                if (report[WiFiNative.StateLink] == (byte)WiFiLinkState.Disconnected)
                {
                    return false;
                }
                if (limit >= 0 && unchecked(Environment.TickCount - start) >= limit)
                {
                    return false;
                }
                Thread.Sleep(AddressPollMilliseconds);
            }
        }

        /// <summary>The network stored on the board, without its secret; null when none is stored.</summary>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        public WiFiStoredNetwork StoredNetwork
        {
            get
            {
                Require();
                byte[] report = new byte[WiFiNative.RecordLength];
                if (WiFiNative.RecordRead(report) == 0)
                {
                    return null;
                }
                return new WiFiStoredNetwork(
                    Encoding.UTF8.GetString(report, WiFiNative.RecordSsid, report[WiFiNative.RecordSsidLength]),
                    (WiFiSecurity)report[WiFiNative.RecordSecurity],
                    (WiFiReconnectionKind)report[WiFiNative.RecordReconnection],
                    (WiFiBootConnection)report[WiFiNative.RecordBoot]);
            }
        }

        /// <summary>
        /// Changes what the stored network does when the board starts. The rest of the stored network
        /// is kept, and the copy it replaces is erased.
        /// </summary>
        /// <param name="boot">What the stored network does when the board starts.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="boot"/> is not a <see cref="WiFiBootConnection"/> value.</exception>
        /// <exception cref="InvalidOperationException">No network is stored on the board.</exception>
        /// <exception cref="IOException">The board could not store the change.</exception>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        public void SetBootConnection(WiFiBootConnection boot)
        {
            Require();
            if (boot != WiFiBootConnection.Background && boot != WiFiBootConnection.BeforeMain && boot != WiFiBootConnection.OnConnect)
            {
                throw new ArgumentOutOfRangeException("boot");
            }
            int written = WiFiNative.RecordSetBoot((int)boot);
            if (written == WiFiNative.WriteNoRecord)
            {
                throw new InvalidOperationException(NoStoredNetwork);
            }
            if (written == WiFiNative.WriteFailed)
            {
                throw new IOException("The board could not store the change to its stored Wi-Fi network.");
            }
        }

        /// <summary>
        /// Erases the network stored on the board, its name and its secret. A network the radio holds
        /// now stays joined until it is dropped.
        /// </summary>
        /// <exception cref="IOException">The board could not erase its storage.</exception>
        /// <exception cref="PlatformNotSupportedException">No radio is running.</exception>
        public void ClearStoredNetwork()
        {
            Require();
            if (WiFiNative.RecordClear() == 0)
            {
                throw new IOException("The board could not erase its stored Wi-Fi network.");
            }
        }

        private static void Require()
        {
            if (!IsSupported)
            {
                throw new PlatformNotSupportedException("No Wi-Fi radio is running on this platform.");
            }
        }

        private static TimeSpan DefaultTimeout()
        {
            return new TimeSpan(DefaultTimeoutMilliseconds * TimeSpan.TicksPerMillisecond);
        }

        private static int Milliseconds(TimeSpan timeout, string name)
        {
            long milliseconds = timeout.Ticks / TimeSpan.TicksPerMillisecond;
            if (timeout.Ticks == -TimeSpan.TicksPerMillisecond)
            {
                return -1;
            }
            if (timeout.Ticks < 0 || milliseconds > int.MaxValue)
            {
                throw new ArgumentOutOfRangeException(name);
            }
            return (int)milliseconds;
        }

        private WiFiConnectionResult Join(
            byte[] ssid,
            ReadOnlySpan<byte> secret,
            WiFiSecurity security,
            WiFiPersistence persistence,
            WiFiReconnectionKind reconnection,
            TimeSpan timeout)
        {
            if (ssid.Length < 1 || ssid.Length > SsidMax)
            {
                throw new ArgumentException(
                    "A network's name is 1 to 32 bytes; this one is " + ssid.Length.ToString() + " bytes.", "ssid");
            }
            CheckKinds(security);
            CheckSecret(secret, (int)security);
            if (persistence != WiFiPersistence.Temporary && persistence != WiFiPersistence.Persistent)
            {
                throw new ArgumentOutOfRangeException("persistence");
            }
            if (reconnection != WiFiReconnectionKind.Automatic && reconnection != WiFiReconnectionKind.Manual)
            {
                throw new ArgumentOutOfRangeException("reconnection");
            }
            int limit = Milliseconds(timeout, "timeout");
            byte[] copy = secret.ToArray();
            try
            {
                int join = WiFiNative.JoinStart(ssid, copy, (int)security, (int)reconnection);
                WiFiConnectionResult result = Follow(join, limit);
                if (result.ConnectionStatus == WiFiConnectionStatus.Success && persistence == WiFiPersistence.Persistent)
                {
                    if (WiFiNative.RecordWrite(ssid, copy, (int)security, (int)reconnection) == WiFiNative.WriteFailed)
                    {
                        throw new IOException("The network was joined, but the board could not store it.");
                    }
                }
                return result;
            }
            finally
            {
                Array.Clear(copy, 0, copy.Length);
            }
        }

        private static void CheckKinds(WiFiSecurity security)
        {
            int bits = (int)security;
            if (bits == 0 || (bits & ~((int)WiFiSecurity.Open | (int)WiFiSecurity.Wpa2 | (int)WiFiSecurity.Wpa3)) != 0)
            {
                throw new ArgumentException("A join names Open, or one or more of Wpa2 and Wpa3.", "security");
            }
            if ((bits & (int)WiFiSecurity.Open) != 0 && bits != (int)WiFiSecurity.Open)
            {
                throw new ArgumentException("Open cannot be combined with a secured kind.", "security");
            }
        }

        private static void CheckSecret(ReadOnlySpan<byte> secret, int kinds)
        {
            if (kinds == (int)WiFiSecurity.Open)
            {
                if (secret.Length != 0)
                {
                    throw new ArgumentException("An open network takes no secret.", "secret");
                }
                return;
            }
            if ((kinds & (int)WiFiSecurity.Wpa2) != 0 && !IsWpa2Secret(secret))
            {
                throw new ArgumentException(
                    "A WPA2 passphrase is 8 to 63 characters, or the key as 64 hexadecimal digits; this secret is "
                        + secret.Length.ToString() + " bytes.",
                    "secret");
            }
            if ((kinds & (int)WiFiSecurity.Wpa3) != 0 && (secret.Length < 1 || secret.Length > Wpa3PasswordMax))
            {
                throw new ArgumentException(
                    "A WPA3 password is 1 to 128 bytes; this secret is " + secret.Length.ToString() + " bytes.", "secret");
            }
        }

        private static bool IsWpa2Secret(ReadOnlySpan<byte> secret)
        {
            if (secret.Length >= Wpa2PassphraseMin && secret.Length <= Wpa2PassphraseMax)
            {
                return true;
            }
            if (secret.Length != Wpa2KeyDigits)
            {
                return false;
            }
            for (int i = 0; i < secret.Length; i++)
            {
                byte b = secret[i];
                bool digit = (b >= (byte)'0' && b <= (byte)'9') || (b >= (byte)'a' && b <= (byte)'f') || (b >= (byte)'A' && b <= (byte)'F');
                if (!digit)
                {
                    return false;
                }
            }
            return true;
        }

        private static WiFiConnectionResult Follow(int join, int limit)
        {
            byte[] report = new byte[WiFiNative.StateLength];
            int start = Environment.TickCount;
            while (true)
            {
                WiFiNative.State(report);
                if (Ended(report, join))
                {
                    break;
                }
                if (limit >= 0 && unchecked(Environment.TickCount - start) >= limit)
                {
                    WiFiNative.Disconnect();
                    WiFiNative.State(report);
                    break;
                }
                Thread.Sleep(PollMilliseconds);
            }
            if (!Ended(report, join))
            {
                return new WiFiConnectionResult(
                    WiFiConnectionStatus.UnspecifiedFailure, WiFiSecurity.None, "another join replaced this one before it ended");
            }
            WiFiConnectionStatus status = (WiFiConnectionStatus)report[WiFiNative.StateOutcome];
            if (status == WiFiConnectionStatus.Success)
            {
                return new WiFiConnectionResult(status, (WiFiSecurity)report[WiFiNative.StateSecurity], null);
            }
            string detail = (report[WiFiNative.StateFlags] & WiFiNative.FlagFailure) != 0 ? Detail(report) : null;
            return new WiFiConnectionResult(status, WiFiSecurity.None, detail);
        }

        private static bool Ended(byte[] report, int join)
        {
            int number = report[WiFiNative.StateJoin]
                | (report[WiFiNative.StateJoin + 1] << 8)
                | (report[WiFiNative.StateJoin + 2] << 16)
                | (report[WiFiNative.StateJoin + 3] << 24);
            return number == join && (report[WiFiNative.StateFlags] & WiFiNative.FlagOutcome) != 0;
        }

        private static string Detail(byte[] report)
        {
            return Encoding.UTF8.GetString(report, WiFiNative.StateDetail, report[WiFiNative.StateDetailLength]);
        }

        private static string HexPairs(byte[] bytes, int start, int count)
        {
            const string digits = "0123456789abcdef";
            char[] text = new char[count * 3 - 1];
            for (int i = 0; i < count; i++)
            {
                byte b = bytes[start + i];
                text[i * 3] = digits[b >> 4];
                text[i * 3 + 1] = digits[b & 15];
                if (i + 1 < count)
                {
                    text[i * 3 + 2] = ':';
                }
            }
            return new string(text);
        }

        private static bool HasWirelessAddress()
        {
            NetworkInterface[] interfaces = NetworkInterface.GetAllNetworkInterfaces();
            for (int i = 0; i < interfaces.Length; i++)
            {
                if (interfaces[i].NetworkInterfaceType == NetworkInterfaceType.Wireless80211
                    && interfaces[i].OperationalStatus == OperationalStatus.Up)
                {
                    return true;
                }
            }
            return false;
        }
    }
}
