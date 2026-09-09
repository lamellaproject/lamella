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

        /// <summary>A <see cref="TimeSpan"/> of zero.</summary>
        public static readonly TimeSpan Zero = new TimeSpan(0);

        /// <summary>The largest representable <see cref="TimeSpan"/>.</summary>
        public static readonly TimeSpan MaxValue = new TimeSpan(Int64.MaxValue);

        /// <summary>The smallest representable <see cref="TimeSpan"/>.</summary>
        public static readonly TimeSpan MinValue = new TimeSpan(Int64.MinValue);

        /// <summary>A span of days, hours, minutes, seconds and milliseconds.</summary>
        /// <exception cref="ArgumentOutOfRangeException">The total is outside the tick range.</exception>
        public TimeSpan(int days, int hours, int minutes, int seconds, int milliseconds)
        {
            long totalMillis = (long)days * 86400000L
                + (long)hours * 3600000L
                + (long)minutes * 60000L
                + (long)seconds * 1000L
                + (long)milliseconds;
            if (totalMillis > Int64.MaxValue / TicksPerMillisecond
                || totalMillis < Int64.MinValue / TicksPerMillisecond)
            {
                throw new ArgumentOutOfRangeException("milliseconds");
            }
            _ticks = totalMillis * TicksPerMillisecond;
        }

        public long Ticks { get { return _ticks; } }

        public int Days { get { return (int)(_ticks / TicksPerDay); } }
        public int Hours { get { return (int)((_ticks / TicksPerHour) % 24); } }
        public int Minutes { get { return (int)((_ticks / TicksPerMinute) % 60); } }
        public int Seconds { get { return (int)((_ticks / TicksPerSecond) % 60); } }
        public int Milliseconds { get { return (int)((_ticks / TicksPerMillisecond) % 1000); } }

#if LAMELLA_SURFACE_FLOAT
        public double TotalDays { get { return (double)_ticks / (double)TicksPerDay; } }
        public double TotalHours { get { return (double)_ticks / (double)TicksPerHour; } }
        public double TotalMinutes { get { return (double)_ticks / (double)TicksPerMinute; } }
        public double TotalSeconds { get { return (double)_ticks / (double)TicksPerSecond; } }
        public double TotalMilliseconds { get { return (double)_ticks / (double)TicksPerMillisecond; } }
#endif

        /// <summary>This span plus <paramref name="ts"/>.</summary>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public TimeSpan Add(TimeSpan ts) { return new TimeSpan(AddTicks(_ticks, ts._ticks)); }

        /// <summary>This span minus <paramref name="ts"/>.</summary>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public TimeSpan Subtract(TimeSpan ts) { return new TimeSpan(SubtractTicks(_ticks, ts._ticks)); }

        private static long AddTicks(long left, long right)
        {
            if (right > 0 && left > Int64.MaxValue - right) { throw new OverflowException("TimeSpan overflowed."); }
            if (right < 0 && left < Int64.MinValue - right) { throw new OverflowException("TimeSpan overflowed."); }
            return left + right;
        }

        private static long SubtractTicks(long left, long right)
        {
            if (right < 0 && left > Int64.MaxValue + right) { throw new OverflowException("TimeSpan overflowed."); }
            if (right > 0 && left < Int64.MinValue + right) { throw new OverflowException("TimeSpan overflowed."); }
            return left - right;
        }

        private static long NegateTicks(long ticks)
        {
            if (ticks == Int64.MinValue) { throw new OverflowException("TimeSpan overflowed."); }
            return -ticks;
        }

#if LAMELLA_SURFACE_FLOAT
        /// <summary>A span of <paramref name="value"/> milliseconds.</summary>
        /// <exception cref="ArgumentException"><paramref name="value"/> is NaN.</exception>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public static TimeSpan FromMilliseconds(double value) { return FromDouble(value, TicksPerMillisecond); }

        /// <summary>A span of <paramref name="value"/> seconds.</summary>
        /// <exception cref="ArgumentException"><paramref name="value"/> is NaN.</exception>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public static TimeSpan FromSeconds(double value) { return FromDouble(value, TicksPerSecond); }

        /// <summary>A span of <paramref name="value"/> minutes.</summary>
        /// <exception cref="ArgumentException"><paramref name="value"/> is NaN.</exception>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public static TimeSpan FromMinutes(double value) { return FromDouble(value, TicksPerMinute); }

        /// <summary>A span of <paramref name="value"/> hours.</summary>
        /// <exception cref="ArgumentException"><paramref name="value"/> is NaN.</exception>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public static TimeSpan FromHours(double value) { return FromDouble(value, TicksPerHour); }

        /// <summary>A span of <paramref name="value"/> days.</summary>
        /// <exception cref="ArgumentException"><paramref name="value"/> is NaN.</exception>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public static TimeSpan FromDays(double value) { return FromDouble(value, TicksPerDay); }

        private static TimeSpan FromDouble(double value, long scale)
        {
            if (Double.IsNaN(value)) { throw new ArgumentException("value"); }
            double ticks = value * (double)scale;
            if (!(ticks >= -9223372036854775808.0 && ticks < 9223372036854775808.0))
            {
                throw new OverflowException("TimeSpan overflowed.");
            }
            return new TimeSpan((long)ticks);
        }
#endif

        public static TimeSpan FromTicks(long value) { return new TimeSpan(value); }

        /// <summary>A span of <paramref name="value"/> whole milliseconds.</summary>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public static TimeSpan FromMilliseconds(long value) { return new TimeSpan(ScaleTicks(value, TicksPerMillisecond)); }

        /// <summary>A span of <paramref name="value"/> whole seconds.</summary>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public static TimeSpan FromSeconds(long value) { return new TimeSpan(ScaleTicks(value, TicksPerSecond)); }

        /// <summary>A span of <paramref name="value"/> whole minutes.</summary>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public static TimeSpan FromMinutes(long value) { return new TimeSpan(ScaleTicks(value, TicksPerMinute)); }

        private static long ScaleTicks(long value, long scale)
        {
            if (value > Int64.MaxValue / scale || value < Int64.MinValue / scale)
            {
                throw new OverflowException("TimeSpan overflowed.");
            }
            return value * scale;
        }

        /// <summary>Compares two spans: -1, 0 or 1.</summary>
        public static int Compare(TimeSpan t1, TimeSpan t2)
        {
            if (t1._ticks < t2._ticks) { return -1; }
            if (t1._ticks > t2._ticks) { return 1; }
            return 0;
        }

        /// <summary>Whether two spans are equal.</summary>
        public static bool Equals(TimeSpan t1, TimeSpan t2) { return t1._ticks == t2._ticks; }

        /// <summary>This span, negated.</summary>
        /// <exception cref="OverflowException">This span is <see cref="MinValue"/>.</exception>
        public TimeSpan Negate() { return new TimeSpan(NegateTicks(_ticks)); }

        /// <summary>The negation of <paramref name="t"/>.</summary>
        /// <exception cref="OverflowException"><paramref name="t"/> is <see cref="MinValue"/>.</exception>
        public static TimeSpan operator -(TimeSpan t) { return new TimeSpan(NegateTicks(t._ticks)); }

        /// <summary>Returns <paramref name="t"/> unchanged.</summary>
        public static TimeSpan operator +(TimeSpan t) { return t; }

        /// <summary>The absolute value of this span.</summary>
        /// <exception cref="OverflowException">This span is <see cref="MinValue"/>.</exception>
        public TimeSpan Duration()
        {
            return new TimeSpan(_ticks < 0 ? NegateTicks(_ticks) : _ticks);
        }

        /// <summary>The sum of two spans.</summary>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public static TimeSpan operator +(TimeSpan left, TimeSpan right) { return new TimeSpan(AddTicks(left._ticks, right._ticks)); }

        /// <summary>The difference of two spans.</summary>
        /// <exception cref="OverflowException">The result is outside the tick range.</exception>
        public static TimeSpan operator -(TimeSpan left, TimeSpan right) { return new TimeSpan(SubtractTicks(left._ticks, right._ticks)); }

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
