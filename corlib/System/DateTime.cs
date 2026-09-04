// Lamella managed corlib (from scratch). -- System.DateTime
namespace System
{
    public struct DateTime : IComparable
    {
        private const long TicksPerMillisecond = 10000;
        private const long TicksPerSecond = 10000000;
        private const long TicksPerMinute = 600000000;
        private const long TicksPerHour = 36000000000;
        private const long TicksPerDay = 864000000000;

        private static readonly int[] DaysToMonth365 =
            { 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365 };
        private static readonly int[] DaysToMonth366 =
            { 0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366 };

        private long _dateData;

        private const int KindShift = 62;
        private const long TicksMask = 0x3FFFFFFFFFFFFFFF;
        private const long KindBitsMask = 3L << KindShift;
        private const long KindBitsUnspecified = 0L;
        private const long KindBitsUtc = 1L << KindShift;
        private const long KindBitsLocal = 2L << KindShift;

        private const long MinTicks = 0;
        private const long MaxTicks = 3155378975999999999;

        private long InternalTicks { get { return _dateData & TicksMask; } }

        private long InternalKindBits { get { return _dateData & KindBitsMask; } }

        private static long CheckedTicks(long ticks)
        {
            if (ticks < MinTicks || ticks > MaxTicks)
            {
                throw new ArgumentOutOfRangeException("Ticks must be between DateTime.MinValue.Ticks and DateTime.MaxValue.Ticks.");
            }
            return ticks;
        }

        private static DateTime FromDateData(long dateData)
        {
            DateTime result = new DateTime(0L);
            result._dateData = dateData;
            return result;
        }

        private DateTime WithTicks(long ticks)
        {
            return FromDateData(CheckedTicks(ticks) | InternalKindBits);
        }

        public static readonly DateTime MinValue = new DateTime(0L);
        public static readonly DateTime MaxValue = new DateTime(DateToTicks(9999, 12, 31) + TicksPerDay - 1);

        public DateTime(long ticks) { _dateData = CheckedTicks(ticks); }

        public DateTime(int year, int month, int day)
        {
            _dateData = CheckedTicks(DateToTicks(year, month, day));
        }

        public DateTime(int year, int month, int day, int hour, int minute, int second)
        {
            _dateData = CheckedTicks(DateToTicks(year, month, day) + TimeToTicks(hour, minute, second));
        }

        public DateTime(int year, int month, int day, int hour, int minute, int second, int millisecond)
        {
            _dateData = CheckedTicks(DateToTicks(year, month, day)
                + TimeToTicks(hour, minute, second)
                + (long)millisecond * TicksPerMillisecond);
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public DateTime(long ticks, DateTimeKind kind)
        {
            if (kind < DateTimeKind.Unspecified || kind > DateTimeKind.Local)
            {
                throw new ArgumentException("Invalid DateTimeKind value.", "kind");
            }
            _dateData = CheckedTicks(ticks) | ((long)kind << KindShift);
        }
#endif

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

        public long Ticks { get { return InternalTicks; } }

        [Lamella.Runtime.RuntimeProvided] private static long NowTicks() { return 0; }

        public static DateTime Now { get { return FromDateData(CheckedTicks(NowTicks()) | KindBitsLocal); } }

        public static DateTime UtcNow { get { return FromDateData(CheckedTicks(NowTicks()) | KindBitsUtc); } }

        public static DateTime Today
        {
            get
            {
                long ticks = CheckedTicks(NowTicks());
                return FromDateData((ticks - (ticks % TicksPerDay)) | KindBitsLocal);
            }
        }

        private int DayNumber { get { return (int)(InternalTicks / TicksPerDay); } }

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

        public int Hour { get { return (int)((InternalTicks / TicksPerHour) % 24); } }
        public int Minute { get { return (int)((InternalTicks / TicksPerMinute) % 60); } }
        public int Second { get { return (int)((InternalTicks / TicksPerSecond) % 60); } }
        public int Millisecond { get { return (int)((InternalTicks / TicksPerMillisecond) % 1000); } }

        public DayOfWeek DayOfWeek { get { return (DayOfWeek)((int)((InternalTicks / TicksPerDay + 1) % 7)); } }

        public DateTime Date { get { long ticks = InternalTicks; return WithTicks(ticks - (ticks % TicksPerDay)); } }

        public TimeSpan TimeOfDay { get { return new TimeSpan(InternalTicks % TicksPerDay); } }

        public DateTime Add(TimeSpan value) { return WithTicks(InternalTicks + value.Ticks); }
        public DateTime AddTicks(long value) { return WithTicks(InternalTicks + value); }
        public TimeSpan Subtract(DateTime value) { return new TimeSpan(InternalTicks - value.InternalTicks); }
        public DateTime Subtract(TimeSpan value) { return WithTicks(InternalTicks - value.Ticks); }

#if LAMELLA_SURFACE_FLOAT
        public DateTime AddDays(double value) { return AddTicks((long)(value * (double)TicksPerDay)); }
        public DateTime AddHours(double value) { return AddTicks((long)(value * (double)TicksPerHour)); }
        public DateTime AddMinutes(double value) { return AddTicks((long)(value * (double)TicksPerMinute)); }
        public DateTime AddSeconds(double value) { return AddTicks((long)(value * (double)TicksPerSecond)); }
        public DateTime AddMilliseconds(double value) { return AddTicks((long)(value * (double)TicksPerMillisecond)); }
#endif

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
            return WithTicks(DateToTicks(y, m, d) + (InternalTicks % TicksPerDay));
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

        public int CompareTo(DateTime value)
        {
            if (InternalTicks < value.InternalTicks) return -1;
            if (InternalTicks > value.InternalTicks) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((DateTime)obj);
        }

        public bool Equals(DateTime value) { return InternalTicks == value.InternalTicks; }

        public override bool Equals(object obj)
        {
            if (obj == null) return false;
            return InternalTicks == ((DateTime)obj).InternalTicks;
        }

        public override int GetHashCode()
        {
            long ticks = InternalTicks;
            return (int)ticks ^ (int)(ticks >> 32);
        }

        public static int Compare(DateTime t1, DateTime t2)
        {
            if (t1.InternalTicks < t2.InternalTicks) return -1;
            if (t1.InternalTicks > t2.InternalTicks) return 1;
            return 0;
        }

        public static bool Equals(DateTime t1, DateTime t2)
        {
            return t1.InternalTicks == t2.InternalTicks;
        }

        public DateTime ToLocalTime()
        {
            if (InternalKindBits == KindBitsLocal) return this;
            return FromDateData(InternalTicks | KindBitsLocal);
        }

        public DateTime ToUniversalTime()
        {
            if (InternalKindBits == KindBitsUtc) return this;
            return FromDateData(InternalTicks | KindBitsUtc);
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public DateTimeKind Kind { get { return (DateTimeKind)(int)((_dateData >> KindShift) & 3L); } }

        public static DateTime SpecifyKind(DateTime value, DateTimeKind kind)
        {
            return new DateTime(value.InternalTicks, kind);
        }
#endif

        public static bool operator ==(DateTime left, DateTime right) { return left.InternalTicks == right.InternalTicks; }
        public static bool operator !=(DateTime left, DateTime right) { return left.InternalTicks != right.InternalTicks; }
        public static bool operator <(DateTime left, DateTime right) { return left.InternalTicks < right.InternalTicks; }
        public static bool operator >(DateTime left, DateTime right) { return left.InternalTicks > right.InternalTicks; }
        public static bool operator <=(DateTime left, DateTime right) { return left.InternalTicks <= right.InternalTicks; }
        public static bool operator >=(DateTime left, DateTime right) { return left.InternalTicks >= right.InternalTicks; }

        public static DateTime operator +(DateTime d, TimeSpan t) { return d.WithTicks(d.InternalTicks + t.Ticks); }
        public static DateTime operator -(DateTime d, TimeSpan t) { return d.WithTicks(d.InternalTicks - t.Ticks); }
        public static TimeSpan operator -(DateTime left, DateTime right) { return new TimeSpan(left.InternalTicks - right.InternalTicks); }

        public static DateTime Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            DateTime parsed;
            if (!TryParseCore(s, out parsed))
            {
                throw new FormatException("String was not recognized as a valid DateTime.");
            }
            return parsed;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out DateTime result)
        {
            return TryParseCore(s, out result);
        }
#endif

        private static bool TryParseCore(string s, out DateTime result)
        {
            result = new DateTime(0L);
            if ((object)s == null) return false;

            int end = s.Length;
            while (end > 0 && Char.IsWhiteSpace(s[end - 1])) end = end - 1;
            int i = 0;
            while (i < end && Char.IsWhiteSpace(s[i])) i = i + 1;
            if (i >= end) return false;

            int firstStart = i;
            int stop = DigitRunEnd(s, i, end);
            if (stop == i) return false;
            int first = DigitValue(s, i, stop);
            int firstLength = stop - i;

            int year = 0;
            int month = 0;
            int day = 0;
            bool haveDate = false;

            if (stop < end && s[stop] == '-')
            {
                if (firstLength != 4) return false;
                year = first;
                i = stop + 1;
                stop = DigitRunEnd(s, i, end);
                if (stop == i || stop - i > 2) return false;
                month = DigitValue(s, i, stop);
                i = stop;
                if (i >= end || s[i] != '-') return false;
                i = i + 1;
                stop = DigitRunEnd(s, i, end);
                if (stop == i || stop - i > 2) return false;
                day = DigitValue(s, i, stop);
                i = stop;
                haveDate = true;
            }
            else if (stop < end && s[stop] == '/')
            {
                if (firstLength > 2) return false;
                month = first;
                i = stop + 1;
                stop = DigitRunEnd(s, i, end);
                if (stop == i || stop - i > 2) return false;
                day = DigitValue(s, i, stop);
                i = stop;
                if (i >= end || s[i] != '/') return false;
                i = i + 1;
                stop = DigitRunEnd(s, i, end);
                if (stop - i != 4) return false;
                year = DigitValue(s, i, stop);
                i = stop;
                haveDate = true;
            }

            long dayTicks;
            if (haveDate)
            {
                if (year < 1 || year > 9999) return false;
                if (month < 1 || month > 12) return false;
                if (day < 1 || day > DaysInMonth(year, month)) return false;
                dayTicks = DateToTicks(year, month, day);
                if (i < end)
                {
                    if (s[i] == 'T') i = i + 1;
                    else if (Char.IsWhiteSpace(s[i])) { while (i < end && Char.IsWhiteSpace(s[i])) i = i + 1; }
                    else return false;
                    if (i >= end) return false;
                }
            }
            else
            {
                long nowTicks = CheckedTicks(NowTicks());
                dayTicks = nowTicks - (nowTicks % TicksPerDay);
                i = firstStart;
            }

            long timeTicks = 0;
            if (i < end)
            {
                if (!TryReadTime(s, end, i, out timeTicks)) return false;
            }
            else if (!haveDate)
            {
                return false;
            }

            result = FromDateData((dayTicks + timeTicks) | KindBitsUnspecified);
            return true;
        }

        private static bool TryReadTime(string s, int end, int i, out long ticks)
        {
            ticks = 0;
            int stop = DigitRunEnd(s, i, end);
            if (stop == i || stop - i > 2) return false;
            int hour = DigitValue(s, i, stop);
            i = stop;
            if (i >= end || s[i] != ':') return false;
            i = i + 1;
            stop = DigitRunEnd(s, i, end);
            if (stop - i != 2) return false;
            int minute = DigitValue(s, i, stop);
            i = stop;

            int second = 0;
            long fraction = 0;
            if (i < end && s[i] == ':')
            {
                i = i + 1;
                stop = DigitRunEnd(s, i, end);
                if (stop - i != 2) return false;
                second = DigitValue(s, i, stop);
                i = stop;
                if (i < end && s[i] == '.')
                {
                    i = i + 1;
                    stop = DigitRunEnd(s, i, end);
                    int length = stop - i;
                    if (length < 1 || length > 7) return false;
                    long scale = TicksPerSecond / 10;
                    for (int j = i; j < stop; j++)
                    {
                        fraction = fraction + (long)(s[j] - '0') * scale;
                        scale = scale / 10;
                    }
                    i = stop;
                }
            }

            bool pm = false;
            bool haveDesignator = false;
            while (i < end && Char.IsWhiteSpace(s[i])) i = i + 1;
            if (i < end)
            {
                if (i + 2 > end) return false;
                char lead = s[i];
                char mark = s[i + 1];
                if (mark != 'M' && mark != 'm') return false;
                if (lead == 'P' || lead == 'p') pm = true;
                else if (lead != 'A' && lead != 'a') return false;
                haveDesignator = true;
                i = i + 2;
                while (i < end && Char.IsWhiteSpace(s[i])) i = i + 1;
            }
            if (i != end) return false;

            if (haveDesignator)
            {
                if (hour < 1 || hour > 12) return false;
                if (pm) { if (hour != 12) hour = hour + 12; }
                else if (hour == 12) hour = 0;
            }
            else if (hour > 23) return false;
            if (minute > 59 || second > 59) return false;

            ticks = TimeToTicks(hour, minute, second) + fraction;
            return true;
        }

        private static int DigitRunEnd(string s, int start, int end)
        {
            int j = start;
            while (j < end && s[j] >= '0' && s[j] <= '9') j = j + 1;
            return j;
        }

        private static int DigitValue(string s, int start, int stop)
        {
            int value = 0;
            for (int j = start; j < stop; j++) value = value * 10 + (s[j] - '0');
            return value;
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

        private static void AppendNumber(System.Text.StringBuilder builder, int value)
        {
            builder.Append(value.ToString());
        }

        public override string ToString()
        {
            return ToString("G", System.Globalization.DateTimeFormatInfo.InvariantInfo);
        }

        public string ToString(string format)
        {
            return ToString(format, System.Globalization.DateTimeFormatInfo.InvariantInfo);
        }

        public string ToString(string format, IFormatProvider provider)
        {
            System.Globalization.DateTimeFormatInfo dtfi = GetFormatInfo(provider);
            if (format == null || format.Length == 0) format = "G";
            string pattern = (format.Length == 1) ? ExpandStandard(format[0], dtfi) : format;
            return Format(this, pattern, dtfi);
        }

        private static System.Globalization.DateTimeFormatInfo GetFormatInfo(IFormatProvider provider)
        {
            if (provider != null)
            {
                object formatInfo = provider.GetFormat(typeof(System.Globalization.DateTimeFormatInfo));
                if (formatInfo != null) return (System.Globalization.DateTimeFormatInfo)formatInfo;
            }
            return System.Globalization.DateTimeFormatInfo.InvariantInfo;
        }

        private static string ExpandStandard(char format, System.Globalization.DateTimeFormatInfo dtfi)
        {
            switch (format)
            {
                case 'd': return dtfi.ShortDatePattern;
                case 'D': return dtfi.LongDatePattern;
                case 't': return dtfi.ShortTimePattern;
                case 'T': return dtfi.LongTimePattern;
                case 'f': return dtfi.LongDatePattern + " " + dtfi.ShortTimePattern;
                case 'F': return dtfi.FullDateTimePattern;
                case 'g': return dtfi.ShortDatePattern + " " + dtfi.ShortTimePattern;
                case 'G': return dtfi.ShortDatePattern + " " + dtfi.LongTimePattern;
                case 's': return "yyyy-MM-ddTHH:mm:ss";
                case 'u': return "yyyy-MM-dd HH:mm:ssZ";
                case 'o':
                case 'O': return "yyyy-MM-ddTHH:mm:ss.fffffff";
                case 'r':
                case 'R': return "ddd, dd MMM yyyy HH:mm:ss 'GMT'";
                case 'm':
                case 'M': return "MMMM dd";
                case 'y':
                case 'Y': return "yyyy MMMM";
                default: throw new FormatException("Invalid format string for DateTime.");
            }
        }

        private static bool IsDateTimeSpecifier(char c)
        {
            return c == 'y' || c == 'M' || c == 'd' || c == 'H' || c == 'h'
                || c == 'm' || c == 's' || c == 't' || c == 'f' || c == 'F';
        }

        private static string Format(DateTime dt, string pattern, System.Globalization.DateTimeFormatInfo dtfi)
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            int i = 0;
            int n = pattern.Length;
            while (i < n)
            {
                char c = pattern[i];
                if (c == '\'' || c == '"')
                {
                    char quote = c;
                    i = i + 1;
                    while (i < n && pattern[i] != quote) { result.Append(pattern[i]); i = i + 1; }
                    if (i < n) i = i + 1;
                }
                else if (c == '\\')
                {
                    if (i + 1 < n) { result.Append(pattern[i + 1]); i = i + 2; }
                    else i = i + 1;
                }
                else if (IsDateTimeSpecifier(c))
                {
                    int count = 1;
                    while (i + count < n && pattern[i + count] == c) count = count + 1;
                    AppendField(result, dt, c, count, dtfi);
                    i = i + count;
                }
                else if (c == '/') { result.Append(dtfi.DateSeparator); i = i + 1; }
                else if (c == ':') { result.Append(dtfi.TimeSeparator); i = i + 1; }
                else { result.Append(c); i = i + 1; }
            }
            return result.ToString();
        }

        private static void AppendField(System.Text.StringBuilder result, DateTime dt, char c, int count, System.Globalization.DateTimeFormatInfo dtfi)
        {
            if (c == 'y')
            {
                int year = dt.Year;
                if (count == 1) AppendNumber(result, year % 100);
                else if (count == 2) AppendPadded(result, year % 100, 2);
                else AppendPadded(result, year, count);
            }
            else if (c == 'M')
            {
                int month = dt.Month;
                if (count == 1) AppendNumber(result, month);
                else if (count == 2) AppendPadded(result, month, 2);
                else if (count == 3) result.Append(dtfi.GetAbbreviatedMonthName(month));
                else result.Append(dtfi.GetMonthName(month));
            }
            else if (c == 'd')
            {
                if (count == 1) AppendNumber(result, dt.Day);
                else if (count == 2) AppendPadded(result, dt.Day, 2);
                else if (count == 3) result.Append(dtfi.GetAbbreviatedDayName(dt.DayOfWeek));
                else result.Append(dtfi.GetDayName(dt.DayOfWeek));
            }
            else if (c == 'H')
            {
                if (count == 1) AppendNumber(result, dt.Hour);
                else AppendPadded(result, dt.Hour, 2);
            }
            else if (c == 'h')
            {
                int h12 = dt.Hour % 12;
                if (h12 == 0) h12 = 12;
                if (count == 1) AppendNumber(result, h12);
                else AppendPadded(result, h12, 2);
            }
            else if (c == 'm')
            {
                if (count == 1) AppendNumber(result, dt.Minute);
                else AppendPadded(result, dt.Minute, 2);
            }
            else if (c == 's')
            {
                if (count == 1) AppendNumber(result, dt.Second);
                else AppendPadded(result, dt.Second, 2);
            }
            else if (c == 't')
            {
                string designator = (dt.Hour < 12) ? dtfi.AMDesignator : dtfi.PMDesignator;
                if (count == 1) { if (designator.Length > 0) result.Append(designator[0]); }
                else result.Append(designator);
            }
            else if (c == 'f' || c == 'F')
            {
                long fraction = dt.Ticks % TicksPerSecond;
                char[] digits = new char[7];
                long f = fraction;
                for (int k = 6; k >= 0; k--) { digits[k] = (char)('0' + (int)(f % 10)); f = f / 10; }
                int take = count;
                if (take > 7) take = 7;
                if (c == 'F') { while (take > 0 && digits[take - 1] == '0') take = take - 1; }
                for (int k = 0; k < take; k++) result.Append(digits[k]);
            }
        }
    }
}
