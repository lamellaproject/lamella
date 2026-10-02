// Lamella managed corlib (from scratch). -- System.PercentEncoding (internal)
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    internal static class PercentEncoding
    {
        internal static string EscapeDataString(string text)
        {
            return Encode(text, false);
        }

        internal static string UrlEncode(string text)
        {
            return Encode(text, true);
        }

        private static bool IsLeftAlone(int c, bool form)
        {
            if ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9')) return true;
            if (c == '-' || c == '_' || c == '.') return true;
            if (form) return c == '!' || c == '*' || c == '(' || c == ')';
            return c == '~';
        }

        private static string Encode(string text, bool form)
        {
            int i = 0;
            while (i < text.Length && IsLeftAlone(text[i], form)) i = i + 1;
            if (i == text.Length) return text;

            byte[] utf8 = System.Text.Encoding.UTF8.GetBytes(text);
            System.Text.StringBuilder result = new System.Text.StringBuilder(utf8.Length + 16);
            for (int b = 0; b < utf8.Length; b++)
            {
                int value = utf8[b];
                if (IsLeftAlone(value, form))
                {
                    result.Append((char)value);
                }
                else if (form && value == ' ')
                {
                    result.Append('+');
                }
                else
                {
                    result.Append('%');
                    result.Append(HexDigit(value >> 4));
                    result.Append(HexDigit(value & 0xF));
                }
            }
            return result.ToString();
        }

        private static char HexDigit(int nibble)
        {
            return (char)(nibble < 10 ? '0' + nibble : 'A' + nibble - 10);
        }
    }
}
#endif
