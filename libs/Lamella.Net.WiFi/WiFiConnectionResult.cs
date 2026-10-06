// Lamella.Net.WiFi -- how one join ended.
namespace Lamella.Net.WiFi
{
    /// <summary>How a join ended: its status, the kind of security in force, and one plain line saying why it failed.</summary>
    public sealed class WiFiConnectionResult
    {
        private readonly WiFiConnectionStatus _status;
        private readonly WiFiSecurity _security;
        private readonly string _detail;

        internal WiFiConnectionResult(WiFiConnectionStatus status, WiFiSecurity security, string detail)
        {
            _status = status;
            _security = security;
            _detail = detail;
        }

        /// <summary>How the join ended. A program tests this value, never <see cref="Detail"/>.</summary>
        public WiFiConnectionStatus ConnectionStatus { get { return _status; } }

        /// <summary>
        /// The one kind of security in force once the join succeeded -- the kind chosen when the join
        /// accepted several -- and <see cref="WiFiSecurity.None"/> when it failed.
        /// </summary>
        public WiFiSecurity Security { get { return _security; } }

        /// <summary>
        /// One line for a person to read, saying what the radio reported when the join failed; null
        /// when it succeeded. Its wording is not part of the contract: a program tests
        /// <see cref="ConnectionStatus"/>.
        /// </summary>
        public string Detail { get { return _detail; } }

        /// <summary>The status, then the kind in force or the detail line. Never a network's name or secret.</summary>
        public override string ToString()
        {
            if (_status == WiFiConnectionStatus.Success)
            {
                return "Success (" + WiFiText.Kinds(_security) + ")";
            }
            return _detail == null ? WiFiText.Status(_status) : WiFiText.Status(_status) + ": " + _detail;
        }
    }
}
