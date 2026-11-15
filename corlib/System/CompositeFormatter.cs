// Lamella managed corlib (from scratch). -- System.CompositeFormatter (internal)
using System.Text;

namespace System
{
    internal sealed class CompositeFormatter
    {
        private CompositeFormatter() { }

        private const int IndexLimit = 1000000;
        private const int WidthLimit = 1000000;

        internal static void Append(StringBuilder result, IFormatProvider provider, string format, object[] args)
        {
            if ((object)format == null) throw new ArgumentNullException("format");
#if LAMELLA_SURFACE_NETFX_2_0
            ICustomFormatter custom = null;
            if ((object)provider != null) custom = (ICustomFormatter)provider.GetFormat(typeof(ICustomFormatter));
#endif
            int length = format.Length;
            int pos = 0;
            while (pos < length)
            {
                char c = format[pos];
                if (c != '{' && c != '}')
                {
                    int start = pos;
                    while (pos < length && format[pos] != '{' && format[pos] != '}') pos = pos + 1;
                    result.Append(format, start, pos - start);
                    continue;
                }
                char next = Advance(format, ref pos);
                if (next == c)
                {
                    result.Append(c);
                    pos = pos + 1;
                    continue;
                }
                if (c == '}') Refuse(pos, "Unexpected closing brace without a corresponding opening brace.");
                int width;
                bool left;
                string itemFormat;
                int index = ReadItem(format, ref pos, out width, out left, out itemFormat);
                if (index >= args.Length) throw new FormatException("Index (zero based) must be greater than or equal to zero and less than the size of the argument list.");
                object arg = args[index];
                string text = null;
#if LAMELLA_SURFACE_NETFX_2_0
                if ((object)custom != null) text = custom.Format(itemFormat, arg, provider);
#endif
                if ((object)text == null) text = TextOf(arg, itemFormat, provider);
                int padding = width - text.Length;
                if (padding > 0 && !left) result.Append(' ', padding);
                result.Append(text);
                if (padding > 0 && left) result.Append(' ', padding);
            }
        }

        private static string TextOf(object arg, string itemFormat, IFormatProvider provider)
        {
            string text = null;
            IFormattable formattable = arg as IFormattable;
            if ((object)formattable != null) text = formattable.ToString(itemFormat, provider);
            else if (arg != null) text = arg.ToString();
            return (object)text == null ? "" : text;
        }

        private static int ReadItem(string format, ref int pos, out int width, out bool left, out string itemFormat)
        {
            width = 0;
            left = false;
            itemFormat = null;
            char c = format[pos];
            int index = c - '0';
            if (index < 0 || index > 9) Refuse(pos, ExpectedDigit);
            c = Advance(format, ref pos);
            while (c >= '0' && c <= '9' && index < IndexLimit)
            {
                index = index * 10 + (c - '0');
                c = Advance(format, ref pos);
            }
            while (c == ' ') c = Advance(format, ref pos);
            if (c == ',')
            {
                c = Advance(format, ref pos);
                while (c == ' ') c = Advance(format, ref pos);
                if (c == '-')
                {
                    left = true;
                    c = Advance(format, ref pos);
                }
                width = c - '0';
                if (width < 0 || width > 9) Refuse(pos, ExpectedDigit);
                c = Advance(format, ref pos);
                while (c >= '0' && c <= '9' && width < WidthLimit)
                {
                    width = width * 10 + (c - '0');
                    c = Advance(format, ref pos);
                }
                while (c == ' ') c = Advance(format, ref pos);
            }
            if (c == ':')
            {
                int start = pos + 1;
                c = Advance(format, ref pos);
                while (c != '}')
                {
                    if (c == '{') Refuse(pos, Unclosed);
                    c = Advance(format, ref pos);
                }
                if (pos > start) itemFormat = format.Substring(start, pos - start);
            }
            else if (c != '}')
            {
                Refuse(pos, Unclosed);
            }
            pos = pos + 1;
            return index;
        }

        private const string ExpectedDigit = "Expected an ASCII digit.";
        private const string Unclosed = "Format item ends prematurely.";

        private static char Advance(string format, ref int pos)
        {
            pos = pos + 1;
            if (pos >= format.Length) Refuse(pos, Unclosed);
            return format[pos];
        }

        private static void Refuse(int offset, string reason)
        {
            throw new FormatException("Input string was not in a correct format. Failure to parse near offset " + NumberFormatter.Decimal(offset, 0) + ". " + reason);
        }
    }
}
