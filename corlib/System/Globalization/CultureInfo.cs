// Lamella managed corlib (from scratch). -- System.Globalization.CultureInfo
namespace System.Globalization
{
    public class CultureInfo : System.IFormatProvider
    {
        private static CultureInfo _invariant;
        private string _name;

        public CultureInfo(string name)
        {
            _name = name;
        }

        public static CultureInfo InvariantCulture
        {
            get
            {
                if ((object)_invariant == null) _invariant = new CultureInfo("");
                return _invariant;
            }
        }

        public static CultureInfo CurrentCulture
        {
            get { return InvariantCulture; }
        }

        public string Name
        {
            get { return _name; }
        }

        public NumberFormatInfo NumberFormat
        {
            get { return NumberFormatInfo.InvariantInfo; }
        }

        public object GetFormat(System.Type formatType)
        {
            return NumberFormatInfo.InvariantInfo;
        }

        public override string ToString()
        {
            return _name;
        }
    }
}
