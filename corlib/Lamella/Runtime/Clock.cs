// Lamella managed corlib (from scratch). -- Lamella.Runtime.Clock
namespace Lamella.Runtime
{
    public sealed class Clock
    {
        private Clock() { }

        private static long anchorMonotonicMillis;
        private static long anchorTicks;

        private static int source;

        public static void SetTicks(long utcTicks)
        {
            SetTicks(utcTicks, ClockSource.Application);
        }

        public static void SetTicks(long utcTicks, ClockSource source)
        {
            Clock.anchorMonotonicMillis = MonotonicMilliseconds();
            Clock.anchorTicks = utcTicks;
            Clock.source = (int)source;
        }

        public static bool IsSet()
        {
            return source != (int)ClockSource.Unset;
        }

        public static ClockSource Source
        {
            get { return (ClockSource)source; }
        }

        internal static int SourceCode()
        {
            return (int)Source;
        }

        internal static long NowTicks()
        {
            long elapsedMillis;
            if (source == (int)ClockSource.Unset)
            {
                elapsedMillis = MonotonicMilliseconds();
                return elapsedMillis * 10000;
            }
            elapsedMillis = MonotonicMilliseconds() - anchorMonotonicMillis;
            if (elapsedMillis < 0)
            {
                elapsedMillis = 0;
            }
            return anchorTicks + elapsedMillis * 10000;
        }

        [Lamella.Runtime.RuntimeProvided]
        [Lamella.Runtime.IntendedDefault]
        private static long MonotonicMilliseconds() { return 0; }
    }
}
