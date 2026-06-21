// Lamella managed corlib (from scratch). -- System.TimeSpan
namespace System
{
    public struct TimeSpan : IComparable
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

        public static TimeSpan FromTicks(long value) { return new TimeSpan(value); }

        public TimeSpan Negate() { return new TimeSpan(-_ticks); }
        public static TimeSpan operator -(TimeSpan t) { return new TimeSpan(-t._ticks); }

        public static TimeSpan operator +(TimeSpan left, TimeSpan right) { return new TimeSpan(left._ticks + right._ticks); }
        public static TimeSpan operator -(TimeSpan left, TimeSpan right) { return new TimeSpan(left._ticks - right._ticks); }

        public static bool operator ==(TimeSpan left, TimeSpan right) { return left._ticks == right._ticks; }
        public static bool operator !=(TimeSpan left, TimeSpan right) { return left._ticks != right._ticks; }
        public static bool operator <(TimeSpan left, TimeSpan right) { return left._ticks < right._ticks; }
        public static bool operator >(TimeSpan left, TimeSpan right) { return left._ticks > right._ticks; }
        public static bool operator <=(TimeSpan left, TimeSpan right) { return left._ticks <= right._ticks; }
        public static bool operator >=(TimeSpan left, TimeSpan right) { return left._ticks >= right._ticks; }

        public int CompareTo(TimeSpan value)
        {
            if (_ticks < value._ticks) return -1;
            if (_ticks > value._ticks) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((TimeSpan)obj);
        }

        public bool Equals(TimeSpan value) { return _ticks == value._ticks; }

        public override bool Equals(object obj)
        {
            if (obj == null) return false;
            return _ticks == ((TimeSpan)obj)._ticks;
        }

        public override int GetHashCode()
        {
            return (int)_ticks ^ (int)(_ticks >> 32);
        }

        private static void AppendPadded(System.Text.StringBuilder builder, int value, int width)
        {
            char[] digits = new char[width];
            int n = value;
            for (int i = width - 1; i >= 0; i--)
            {
                digits[i] = (char)('0' + n % 10);
                n = n / 10;
            }
            for (int i = 0; i < width; i++) builder.Append(digits[i]);
        }

        public override string ToString()
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            long ticks = _ticks;
            if (ticks < 0)
            {
                result.Append('-');
                ticks = -ticks;
            }
            long days = ticks / TicksPerDay;
            long rest = ticks % TicksPerDay;
            int hours = (int)(rest / TicksPerHour);
            int minutes = (int)((rest / TicksPerMinute) % 60);
            int seconds = (int)((rest / TicksPerSecond) % 60);
            int fraction = (int)(rest % TicksPerSecond);
            if (days != 0)
            {
                result.Append(days);
                result.Append('.');
            }
            AppendPadded(result, hours, 2);
            result.Append(':');
            AppendPadded(result, minutes, 2);
            result.Append(':');
            AppendPadded(result, seconds, 2);
            if (fraction != 0)
            {
                result.Append('.');
                AppendPadded(result, fraction, 7);
            }
            return result.ToString();
        }
    }
}
