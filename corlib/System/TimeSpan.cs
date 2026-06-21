// Lamella managed corlib (from scratch). -- System.TimeSpan
namespace System
{
    public struct TimeSpan
    {
        public const long TicksPerMillisecond = 10000;
        public const long TicksPerSecond = 10000000;
        public const long TicksPerMinute = 600000000;
        public const long TicksPerHour = 36000000000;
        public const long TicksPerDay = 864000000000;

        private long _ticks;

        public TimeSpan(long ticks) { _ticks = ticks; }

        public TimeSpan(int hours, int minutes, int seconds)
        {
            long totalSeconds = (long)hours * 3600 + (long)minutes * 60 + (long)seconds;
            _ticks = totalSeconds * TicksPerSecond;
        }

        public TimeSpan(int days, int hours, int minutes, int seconds)
        {
            long totalSeconds = ((long)days * 24 + hours) * 3600 + (long)minutes * 60 + seconds;
            _ticks = totalSeconds * TicksPerSecond;
        }

        public long Ticks { get { return _ticks; } }

        public int Days { get { return (int)(_ticks / TicksPerDay); } }
        public int Hours { get { return (int)((_ticks / TicksPerHour) % 24); } }
        public int Minutes { get { return (int)((_ticks / TicksPerMinute) % 60); } }
        public int Seconds { get { return (int)((_ticks / TicksPerSecond) % 60); } }
        public int Milliseconds { get { return (int)((_ticks / TicksPerMillisecond) % 1000); } }

        public double TotalDays { get { return (double)_ticks / (double)TicksPerDay; } }
        public double TotalHours { get { return (double)_ticks / (double)TicksPerHour; } }
        public double TotalMinutes { get { return (double)_ticks / (double)TicksPerMinute; } }
        public double TotalSeconds { get { return (double)_ticks / (double)TicksPerSecond; } }
        public double TotalMilliseconds { get { return (double)_ticks / (double)TicksPerMillisecond; } }

        public TimeSpan Add(TimeSpan ts) { return new TimeSpan(_ticks + ts._ticks); }
        public TimeSpan Subtract(TimeSpan ts) { return new TimeSpan(_ticks - ts._ticks); }

        public static TimeSpan FromMilliseconds(double value) { return new TimeSpan((long)(value * (double)TicksPerMillisecond)); }
        public static TimeSpan FromSeconds(double value) { return new TimeSpan((long)(value * (double)TicksPerSecond)); }
        public static TimeSpan FromMinutes(double value) { return new TimeSpan((long)(value * (double)TicksPerMinute)); }
        public static TimeSpan FromHours(double value) { return new TimeSpan((long)(value * (double)TicksPerHour)); }
        public static TimeSpan FromDays(double value) { return new TimeSpan((long)(value * (double)TicksPerDay)); }
    }
}
