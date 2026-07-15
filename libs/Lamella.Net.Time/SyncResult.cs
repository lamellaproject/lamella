// Lamella.Net.Time -- the report a time-sync attempt returns.
using System;

namespace Lamella.Net.Time
{
    public sealed class SyncResult
    {
        private readonly bool _succeeded;
        private readonly string _protocol;
        private readonly bool _authenticated;
        private readonly int _stratum;
        private readonly TimeSpan _offset;
        private readonly TimeSpan _roundTrip;
        private readonly DateTime _utcTime;
        private readonly string _warning;

        internal SyncResult(
            bool succeeded,
            string protocol,
            bool authenticated,
            int stratum,
            TimeSpan offset,
            TimeSpan roundTrip,
            DateTime utcTime,
            string warning)
        {
            _succeeded = succeeded;
            _protocol = protocol;
            _authenticated = authenticated;
            _stratum = stratum;
            _offset = offset;
            _roundTrip = roundTrip;
            _utcTime = utcTime;
            _warning = warning;
        }

        public bool Succeeded { get { return _succeeded; } }

        public string Protocol { get { return _protocol; } }

        public bool Authenticated { get { return _authenticated; } }

        public int Stratum { get { return _stratum; } }

        public TimeSpan Offset { get { return _offset; } }

        public TimeSpan RoundTrip { get { return _roundTrip; } }

        public DateTime UtcTime { get { return _utcTime; } }

        public string Warning { get { return _warning; } }
    }
}
