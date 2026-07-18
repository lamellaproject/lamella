// Lamella.Net.Time -- the developer-driven wall-clock sync surface.
using System;

namespace Lamella.Net.Time
{
    public sealed class SystemClock
    {
        private SystemClock() { }

        public static DateTime UtcNow
        {
            get { return DateTime.UtcNow; }
        }

        public static bool IsTimeTrusted
        {
            get { return Lamella.Runtime.Clock.IsSet(); }
        }

        public static void Seed(DateTime utc)
        {
            Lamella.Runtime.Clock.SetTicks(utc.Ticks);
        }

        public static SyncResult SyncOnce(string server)
        {
            return new SntpClient(server).SyncOnce();
        }

        internal static void SetUtc(DateTime utc)
        {
            Lamella.Runtime.Clock.SetTicks(utc.Ticks);
        }
    }
}
