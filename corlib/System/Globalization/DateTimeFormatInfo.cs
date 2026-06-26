// Lamella managed corlib (from scratch). -- System.Globalization.DateTimeFormatInfo
namespace System.Globalization
{
    public sealed class DateTimeFormatInfo : IFormatProvider
    {
        private string[] _monthNames;
        private string[] _abbreviatedMonthNames;
        private string[] _dayNames;
        private string[] _abbreviatedDayNames;
        private string _amDesignator;
        private string _pmDesignator;
        private string _dateSeparator;
        private string _timeSeparator;
        private string _shortDatePattern;
        private string _longDatePattern;
        private string _shortTimePattern;
        private string _longTimePattern;
        private string _fullDateTimePattern;

        private DateTimeFormatInfo() { }

        private static readonly DateTimeFormatInfo invariant = CreateInvariant();

        public static DateTimeFormatInfo InvariantInfo { get { return invariant; } }
        public static DateTimeFormatInfo CurrentInfo { get { return invariant; } }

        private static DateTimeFormatInfo CreateInvariant()
        {
            DateTimeFormatInfo info = new DateTimeFormatInfo();
            info._monthNames = new string[] {
                "January", "February", "March", "April", "May", "June", "July", "August",
                "September", "October", "November", "December", "" };
            info._abbreviatedMonthNames = new string[] {
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug",
                "Sep", "Oct", "Nov", "Dec", "" };
            info._dayNames = new string[] {
                "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday" };
            info._abbreviatedDayNames = new string[] {
                "Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat" };
            info._amDesignator = "AM";
            info._pmDesignator = "PM";
            info._dateSeparator = "/";
            info._timeSeparator = ":";
            info._shortDatePattern = "MM/dd/yyyy";
            info._longDatePattern = "dddd, dd MMMM yyyy";
            info._shortTimePattern = "HH:mm";
            info._longTimePattern = "HH:mm:ss";
            info._fullDateTimePattern = "dddd, dd MMMM yyyy HH:mm:ss";
            return info;
        }

        public string[] MonthNames { get { return _monthNames; } }
        public string[] AbbreviatedMonthNames { get { return _abbreviatedMonthNames; } }
        public string[] DayNames { get { return _dayNames; } }
        public string[] AbbreviatedDayNames { get { return _abbreviatedDayNames; } }

        public string AMDesignator { get { return _amDesignator; } }
        public string PMDesignator { get { return _pmDesignator; } }
        public string DateSeparator { get { return _dateSeparator; } }
        public string TimeSeparator { get { return _timeSeparator; } }

        public string ShortDatePattern { get { return _shortDatePattern; } }
        public string LongDatePattern { get { return _longDatePattern; } }
        public string ShortTimePattern { get { return _shortTimePattern; } }
        public string LongTimePattern { get { return _longTimePattern; } }
        public string FullDateTimePattern { get { return _fullDateTimePattern; } }

        public string GetMonthName(int month)
        {
            if (month < 1 || month > 13) throw new ArgumentOutOfRangeException("month");
            return _monthNames[month - 1];
        }

        public string GetAbbreviatedMonthName(int month)
        {
            if (month < 1 || month > 13) throw new ArgumentOutOfRangeException("month");
            return _abbreviatedMonthNames[month - 1];
        }

        public string GetDayName(DayOfWeek dayofweek)
        {
            int day = (int)dayofweek;
            if (day < 0 || day > 6) throw new ArgumentOutOfRangeException("dayofweek");
            return _dayNames[day];
        }

        public string GetAbbreviatedDayName(DayOfWeek dayofweek)
        {
            int day = (int)dayofweek;
            if (day < 0 || day > 6) throw new ArgumentOutOfRangeException("dayofweek");
            return _abbreviatedDayNames[day];
        }

        public object GetFormat(Type formatType)
        {
            if (formatType == typeof(DateTimeFormatInfo)) return this;
            return null;
        }
    }
}
