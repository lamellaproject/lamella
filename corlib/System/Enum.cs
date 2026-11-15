// Lamella managed corlib (from scratch). -- System.Enum
namespace System
{
    public abstract class Enum : ValueType, IFormattable
    {
        /// <summary>Formats this value with <paramref name="format"/>. The provider is not used.</summary>
        /// <param name="format">"G", "D", "X" or "F", or null or empty for "G".</param>
        /// <param name="provider">Not used.</param>
        /// <returns>The text.</returns>
        [Obsolete("The provider argument is not used. Use ToString(String) instead.")]
        public string ToString(string format, IFormatProvider provider)
        {
            bool general = (object)format == null || format.Length == 0;
            if (!general)
            {
                char specifier = format.Length == 1 ? format[0] : ' ';
                general = specifier == 'G' || specifier == 'g';
                if (!general && specifier != 'X' && specifier != 'x' && specifier != 'F' && specifier != 'f' && specifier != 'D' && specifier != 'd')
                {
                    throw new FormatException("Format string can be only \"G\", \"g\", \"X\", \"x\", \"F\", \"f\", \"D\" or \"d\".");
                }
            }
            string text = ToString(format);
            if ((object)text != null) { return text; }
            if (general) { return ToString(); }
            throw new NotSupportedException("This format of an enum is not available on this tier.");
        }

        /// <summary>Formats this value in the general format. The provider is not used.</summary>
        /// <param name="provider">Not used.</param>
        /// <returns>The text.</returns>
        [Obsolete("The provider argument is not used. Use ToString() instead.")]
        public string ToString(IFormatProvider provider)
        {
            return ToString();
        }

#if LAMELLA_SURFACE_NETFX_4_0
        [Lamella.Runtime.RuntimeProvided] public bool HasFlag(Enum flag) { return false; }
#endif
        [Lamella.Runtime.RuntimeProvided] public string ToString(string format) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static object Parse(Type enumType, string value) { return null; }
        [Lamella.Runtime.RuntimeProvided] public static bool IsDefined(Type enumType, object value) { return false; }
        [Lamella.Runtime.RuntimeProvided] public static string GetName(Type enumType, object value) { return null; }
        [Lamella.Runtime.RuntimeProvided] public static string[] GetNames(Type enumType) { return null; }
        [Lamella.Runtime.RuntimeProvided] public static Array GetValues(Type enumType) { return null; }
        [Lamella.Runtime.RuntimeProvided] public static string Format(Type enumType, object value, string format) { return null; }
    }
}
