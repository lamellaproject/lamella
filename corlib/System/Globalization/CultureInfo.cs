// Lamella managed corlib (from scratch). -- System.Globalization.CultureInfo
namespace System.Globalization
{
    public class CultureInfo : System.IFormatProvider
    {
        private const string InvariantOnly = "Only the invariant culture is supported in globalization-invariant mode. See https://aka.ms/GlobalizationInvariantMode for more information.";
        private static CultureInfo _invariant;
        private string _name;

        private CultureInfo()
        {
            _name = "";
        }

        /// <summary>Initializes a culture by name. Only the invariant culture is available, and its name
        /// is the empty string.</summary>
        /// <param name="name">The name of the culture.</param>
        public CultureInfo(string name)
        {
            if ((object)name == null) throw new ArgumentNullException("name");
            if (name.Length != 0)
            {
#if LAMELLA_SURFACE_NETFX_4_0
                throw new CultureNotFoundException("name", name, InvariantOnly);
#else
                throw new ArgumentException(InvariantOnly, "name");
#endif
            }
            _name = name;
        }

        public static CultureInfo InvariantCulture
        {
            get
            {
                if ((object)_invariant == null) _invariant = new CultureInfo();
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

        public virtual object GetFormat(System.Type formatType)
        {
            if (formatType == typeof(NumberFormatInfo)) return NumberFormatInfo.InvariantInfo;
            if (formatType == typeof(DateTimeFormatInfo)) return DateTimeFormatInfo.InvariantInfo;
            return null;
        }

        public override string ToString()
        {
            return _name;
        }
    }
}
