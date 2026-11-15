// Lamella managed corlib (from scratch). -- System.CustomNumberFormat (internal)
using System.Text;

namespace System
{
    internal sealed class CustomNumberFormat
    {
        private CustomNumberFormat() { }

        internal static string Render(string format, bool negative, string digits, int scale, bool floating)
        {
            int section = FindSection(format, digits.Length == 0 ? 2 : (negative ? 1 : 0));
            Shape shape = null;
            while (true)
            {
                shape = Read(format, section);
                if (digits.Length == 0)
                {
                    if (!floating) negative = false;
                    scale = 0;
                    break;
                }
                scale = scale + shape.ScaleShift;
                int position = shape.Scientific ? shape.Places : scale + shape.Places - shape.Point;
                digits = NumberFormatter.RoundDigits(digits, position, ref scale);
                if (digits.Length != 0) break;
                int zeroSection = FindSection(format, 2);
                if (zeroSection == section)
                {
                    if (!floating) negative = false;
                    break;
                }
                section = zeroSection;
            }
            return Emit(format, section, shape, negative, digits, scale);
        }

        private sealed class Shape
        {
            internal int Places;
            internal int Point = -1;
            internal int FirstZero = 0x7FFFFFFF;
            internal int LastZero;
            internal int ScaleShift;
            internal bool Scientific;
            internal bool Grouped;
        }

        private static bool Ends(string format, int at)
        {
            return at >= format.Length || format[at] == '\0';
        }

        private static int FindSection(string format, int which)
        {
            if (which == 0) return 0;
            int at = 0;
            while (at < format.Length)
            {
                char c = format[at];
                at = at + 1;
                if (c == '\0') return 0;
                if (c == '\'' || c == '"')
                {
                    while (!Ends(format, at))
                    {
                        char q = format[at];
                        at = at + 1;
                        if (q == c) break;
                    }
                }
                else if (c == '\\')
                {
                    if (!Ends(format, at)) at = at + 1;
                }
                else if (c == ';')
                {
                    which = which - 1;
                    if (which == 0)
                    {
                        if (!Ends(format, at) && format[at] != ';') return at;
                        return 0;
                    }
                }
            }
            return 0;
        }

        private static Shape Read(string format, int section)
        {
            Shape shape = new Shape();
            int commaAt = -1;
            int commaRun = 0;
            int at = section;
            while (!Ends(format, at) && format[at] != ';')
            {
                char c = format[at];
                at = at + 1;
                if (c == '#')
                {
                    shape.Places = shape.Places + 1;
                }
                else if (c == '0')
                {
                    if (shape.FirstZero == 0x7FFFFFFF) shape.FirstZero = shape.Places;
                    shape.Places = shape.Places + 1;
                    shape.LastZero = shape.Places;
                }
                else if (c == '.')
                {
                    if (shape.Point < 0) shape.Point = shape.Places;
                }
                else if (c == ',')
                {
                    if (shape.Places > 0 && shape.Point < 0)
                    {
                        if (commaAt >= 0 && commaAt == shape.Places)
                        {
                            commaRun = commaRun + 1;
                        }
                        else
                        {
                            if (commaAt >= 0) shape.Grouped = true;
                            commaAt = shape.Places;
                            commaRun = 1;
                        }
                    }
                }
                else if (c == '%')
                {
                    shape.ScaleShift = shape.ScaleShift + 2;
                }
                else if (c == '‰')
                {
                    shape.ScaleShift = shape.ScaleShift + 3;
                }
                else if (c == '\'' || c == '"')
                {
                    while (!Ends(format, at))
                    {
                        char q = format[at];
                        at = at + 1;
                        if (q == c) break;
                    }
                }
                else if (c == '\\')
                {
                    if (!Ends(format, at)) at = at + 1;
                }
                else if (c == 'E' || c == 'e')
                {
                    if (ExponentFollows(format, at))
                    {
                        at = at + 1;
                        while (at < format.Length && format[at] == '0') at = at + 1;
                        shape.Scientific = true;
                    }
                }
            }
            if (shape.Point < 0) shape.Point = shape.Places;
            if (commaAt >= 0)
            {
                if (commaAt == shape.Point) shape.ScaleShift = shape.ScaleShift - commaRun * 3;
                else shape.Grouped = true;
            }
            return shape;
        }

        private static bool ExponentFollows(string format, int at)
        {
            if (at < format.Length && format[at] == '0') return true;
            return at + 1 < format.Length && (format[at] == '+' || format[at] == '-') && format[at + 1] == '0';
        }

        private static string Emit(string format, int section, Shape shape, bool negative, string digits, int scale)
        {
            int zerosFrom = shape.FirstZero < shape.Point ? shape.Point - shape.FirstZero : 0;
            int zerosTo = shape.LastZero > shape.Point ? shape.Point - shape.LastZero : 0;
            int place;
            int surplus;
            if (shape.Scientific)
            {
                place = shape.Point;
                surplus = 0;
            }
            else
            {
                place = scale > shape.Point ? scale : shape.Point;
                surplus = scale - shape.Point;
            }
            int cursor = 0;
            StringBuilder result = new StringBuilder();
            bool leadingSign = negative && section == 0 && scale != 0;
            if (leadingSign) result.Append('-');
            bool pointWritten = false;
            bool scientific = shape.Scientific;
            int at = section;
            while (!Ends(format, at) && format[at] != ';')
            {
                char c = format[at];
                at = at + 1;
                if (surplus > 0 && (c == '#' || c == '0' || c == '.'))
                {
                    while (surplus > 0)
                    {
                        result.Append(cursor < digits.Length ? digits[cursor] : '0');
                        cursor = cursor + 1;
                        if (shape.Grouped && place > 1 && (place - 1) % 3 == 0) result.Append(',');
                        place = place - 1;
                        surplus = surplus - 1;
                    }
                }
                if (c == '#' || c == '0')
                {
                    bool shown = true;
                    char digit = '0';
                    if (surplus < 0)
                    {
                        surplus = surplus + 1;
                        shown = place <= zerosFrom;
                    }
                    else if (cursor < digits.Length)
                    {
                        digit = digits[cursor];
                        cursor = cursor + 1;
                    }
                    else
                    {
                        shown = place > zerosTo;
                    }
                    if (shown)
                    {
                        result.Append(digit);
                        if (shape.Grouped && place > 1 && (place - 1) % 3 == 0) result.Append(',');
                    }
                    place = place - 1;
                }
                else if (c == '.')
                {
                    if (place == 0 && !pointWritten && (zerosTo < 0 || (shape.Point < shape.Places && cursor < digits.Length)))
                    {
                        result.Append('.');
                        pointWritten = true;
                    }
                }
                else if (c == ',')
                {
                }
                else if (c == '\'' || c == '"')
                {
                    while (!Ends(format, at) && format[at] != c)
                    {
                        result.Append(format[at]);
                        at = at + 1;
                    }
                    if (!Ends(format, at)) at = at + 1;
                }
                else if (c == '\\')
                {
                    if (!Ends(format, at))
                    {
                        result.Append(format[at]);
                        at = at + 1;
                    }
                }
                else if (c == 'E' || c == 'e')
                {
                    if (scientific && ExponentFollows(format, at))
                    {
                        bool plus = format[at] == '+';
                        int minimum = format[at] == '0' ? 1 : 0;
                        at = at + 1;
                        while (at < format.Length && format[at] == '0')
                        {
                            minimum = minimum + 1;
                            at = at + 1;
                        }
                        if (minimum > 10) minimum = 10;
                        AppendExponent(result, c, digits.Length == 0 ? 0 : scale - shape.Point, minimum, plus);
                        scientific = false;
                    }
                    else
                    {
                        result.Append(c);
                        if (at < format.Length && (format[at] == '+' || format[at] == '-'))
                        {
                            result.Append(format[at]);
                            at = at + 1;
                        }
                        while (at < format.Length && format[at] == '0')
                        {
                            result.Append('0');
                            at = at + 1;
                        }
                    }
                }
                else
                {
                    result.Append(c);
                }
            }
            if (negative && section == 0 && scale == 0 && result.Length > 0) return "-" + result.ToString();
            return result.ToString();
        }

        private static void AppendExponent(StringBuilder result, char letter, int exponent, int minimum, bool plus)
        {
            result.Append(letter);
            if (exponent < 0) result.Append('-');
            else if (plus) result.Append('+');
            result.Append(NumberFormatter.Decimal(exponent < 0 ? -exponent : exponent, minimum));
        }
    }
}
