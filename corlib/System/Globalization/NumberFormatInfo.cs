// Lamella managed corlib (from scratch). -- System.Globalization.NumberFormatInfo
namespace System.Globalization
{
    public class NumberFormatInfo : System.IFormatProvider
    {
        private static NumberFormatInfo _invariant;

        public NumberFormatInfo() { }

        public static NumberFormatInfo InvariantInfo
        {
            get
            {
                if ((object)_invariant == null) _invariant = new NumberFormatInfo();
                return _invariant;
            }
        }

        public static NumberFormatInfo CurrentInfo
        {
            get { return InvariantInfo; }
        }

        public string NumberDecimalSeparator
        {
            get { return "."; }
        }

        public string NumberGroupSeparator
        {
            get { return ","; }
        }

        public int NumberDecimalDigits
        {
            get { return 2; }
        }

        public string NegativeSign
        {
            get { return "-"; }
        }

        public string PositiveSign
        {
            get { return "+"; }
        }

        public object GetFormat(System.Type formatType)
        {
            return this;
        }
    }
}
