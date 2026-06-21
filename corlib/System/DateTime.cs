// Lamella managed corlib (from scratch). -- System.DateTime
namespace System
{
    public struct DateTime : IComparable
    {
        public const long TicksPerMillisecond = 10000;
        public const long TicksPerSecond = 10000000;
        public const long TicksPerMinute = 600000000;
        public const long TicksPerHour = 36000000000;
        public const long TicksPerDay = 864000000000;

        private static readonly int[] DaysToMonth365 =
            { 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365 };
        private static readonly int[] DaysToMonth366 =
            { 0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366 };

        private long _ticks;

        public static readonly DateTime MinValue = new DateTime(0L);
        public static readonly DateTime MaxValue = new DateTime(DateToTicks(9999, 12, 31) + TicksPerDay - 1);

        public DateTime(long ticks) { _ticks = ticks; }

        public DateTime(int year, int month, int day)
        {
            _ticks = DateToTicks(year, month, day);
        }

        public DateTime(int year, int month, int day, int hour, int minute, int second)
        {
            _ticks = DateToTicks(year, month, day) + TimeToTicks(hour, minute, second);
        }

        public static bool IsLeapYear(int year)
        {
            return (year % 4 == 0) && ((year % 100 != 0) || (year % 400 == 0));
        }

        private static long DateToTicks(int year, int month, int day)
        {
            int[] days = IsLeapYear(year) ? DaysToMonth366 : DaysToMonth365;
            int y = year - 1;
            int n = y * 365 + y / 4 - y / 100 + y / 400 + days[month - 1] + day - 1;
            return n * TicksPerDay;
        }

        private static long TimeToTicks(int hour, int minute, int second)
        {
            long totalSeconds = (long)hour * 3600 + (long)minute * 60 + (long)second;
            return totalSeconds * TicksPerSecond;
        }

        public long Ticks { get { return _ticks; } }

        private int DayNumber { get { return (int)(_ticks / TicksPerDay); } }

        private int GetDatePart(int part)
        {
            int n = DayNumber;
            int y400 = n / 146097;
            n -= y400 * 146097;
            int y100 = n / 36524;
            if (y100 == 4) y100 = 3;
            n -= y100 * 36524;
            int y4 = n / 1461;
            n -= y4 * 1461;
            int y1 = n / 365;
            if (y1 == 4) y1 = 3;

            int year = y400 * 400 + y100 * 100 + y4 * 4 + y1 + 1;
            if (part == 0) return year;

            n -= y1 * 365;
            if (part == 3) return n + 1;

            int[] days = (y1 == 3 && (y4 != 24 || y100 == 3)) ? DaysToMonth366 : DaysToMonth365;
            int m = (n >> 5) + 1;
            while (n >= days[m]) m++;
            if (part == 1) return m;

            return n - days[m - 1] + 1;
        }

        public int Year { get { return GetDatePart(0); } }
        public int Month { get { return GetDatePart(1); } }
        public int Day { get { return GetDatePart(2); } }
        public int DayOfYear { get { return GetDatePart(3); } }

        public int Hour { get { return (int)((_ticks / TicksPerHour) % 24); } }
        public int Minute { get { return (int)((_ticks / TicksPerMinute) % 60); } }
        public int Second { get { return (int)((_ticks / TicksPerSecond) % 60); } }
        public int Millisecond { get { return (int)((_ticks / TicksPerMillisecond) % 1000); } }

        public DayOfWeek DayOfWeek { get { return (DayOfWeek)((int)((_ticks / TicksPerDay + 1) % 7)); } }

        public DateTime Date { get { return new DateTime(_ticks - (_ticks % TicksPerDay)); } }

        public TimeSpan TimeOfDay { get { return new TimeSpan(_ticks % TicksPerDay); } }

        public DateTime Add(TimeSpan value) { return new DateTime(_ticks + value.Ticks); }
        public DateTime AddTicks(long value) { return new DateTime(_ticks + value); }
        public TimeSpan Subtract(DateTime value) { return new TimeSpan(_ticks - value._ticks); }
        public DateTime Subtract(TimeSpan value) { return new DateTime(_ticks - value.Ticks); }

        public DateTime AddDays(double value) { return AddTicks((long)(value * (double)TicksPerDay)); }
        public DateTime AddHours(double value) { return AddTicks((long)(value * (double)TicksPerHour)); }
        public DateTime AddMinutes(double value) { return AddTicks((long)(value * (double)TicksPerMinute)); }
        public DateTime AddSeconds(double value) { return AddTicks((long)(value * (double)TicksPerSecond)); }
        public DateTime AddMilliseconds(double value) { return AddTicks((long)(value * (double)TicksPerMillisecond)); }

        public DateTime AddMonths(int months)
        {
            int y = GetDatePart(0);
            int m = GetDatePart(1);
            int d = GetDatePart(2);
            int i = m - 1 + months;
            if (i >= 0)
            {
                m = i % 12 + 1;
                y = y + i / 12;
            }
            else
            {
                m = 12 + (i + 1) % 12;
                y = y + (i - 11) / 12;
            }
            int daysInMonth = DaysInMonth(y, m);
            if (d > daysInMonth) d = daysInMonth;
            return new DateTime(DateToTicks(y, m, d) + (_ticks % TicksPerDay));
        }

        public DateTime AddYears(int value)
        {
            return AddMonths(value * 12);
        }

        public static int DaysInMonth(int year, int month)
        {
            int[] days = IsLeapYear(year) ? DaysToMonth366 : DaysToMonth365;
            return days[month] - days[month - 1];
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            DateTime other = (DateTime)obj;
            if (_ticks < other._ticks) return -1;
            if (_ticks > other._ticks) return 1;
            return 0;
        }

        public override bool Equals(object obj)
        {
            if (obj == null) return false;
            return _ticks == ((DateTime)obj)._ticks;
        }

        public override int GetHashCode()
        {
            return (int)_ticks ^ (int)(_ticks >> 32);
        }
    }
}
