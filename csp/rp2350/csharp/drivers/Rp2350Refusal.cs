// What the RP2350 drivers say when they refuse: each text is a template this fills in.
using System;

internal sealed class Rp2350Refusal
{
    private Rp2350Refusal() { }

    /// <summary>Throws <see cref="InvalidOperationException"/> with <paramref name="text"/>, in which <c>#</c>,
    /// <c>@</c> and <c>%</c> stand for <paramref name="first"/>, <paramref name="second"/> and
    /// <paramref name="third"/> in decimal, none of them negative, and <c>~</c> stands for
    /// <paramref name="letter"/>.</summary>
    internal static void Refuse(string text, int first, int second, int third, char letter)
    {
        char[] said = new char[text.Length + 64];
        int length = 0;
        for (int index = 0; index < text.Length; index = index + 1)
        {
            char unit = text[index];
            if (unit == '#' || unit == '@' || unit == '%')
            {
                int number = unit == '#' ? first : unit == '@' ? second : third;
                int place = 1;
                while (number / place >= 10)
                {
                    place = place * 10;
                }
                for (; place > 0; place = place / 10)
                {
                    said[length] = (char)('0' + number / place % 10);
                    length = length + 1;
                }
            }
            else
            {
                said[length] = unit == '~' ? letter : unit;
                length = length + 1;
            }
        }
        throw new InvalidOperationException(new string(said, 0, length));
    }
}
