// Lamella managed corlib (from scratch). -- System.Decimal
namespace System
{
    public struct Decimal : IComparable, IFormattable
    {
        private int lo;
        private int mid;
        private int hi;
        private int flags;

        public static readonly Decimal Zero = new Decimal(0, 0, 0, false, 0);
        public static readonly Decimal One = new Decimal(1, 0, 0, false, 0);
        public static readonly Decimal MinusOne = new Decimal(1, 0, 0, true, 0);
        public static readonly Decimal MaxValue = new Decimal(-1, -1, -1, false, 0);
        public static readonly Decimal MinValue = new Decimal(-1, -1, -1, true, 0);

        public Decimal(int lo, int mid, int hi, bool isNegative, byte scale)
        {
            if (scale > 28) throw new ArgumentOutOfRangeException("Decimal's scale value must be between 0 and 28, inclusive.");
            this.lo = lo;
            this.mid = mid;
            this.hi = hi;
            this.flags = ((int)scale << 16) | (isNegative ? unchecked((int)0x80000000) : 0);
        }

        public Decimal(int value)
        {
            this.mid = 0;
            this.hi = 0;
            if (value < 0)
            {
                this.lo = (int)(-(long)value);
                this.flags = unchecked((int)0x80000000);
            }
            else
            {
                this.lo = value;
                this.flags = 0;
            }
        }

        public Decimal(uint value)
        {
            this.lo = (int)value;
            this.mid = 0;
            this.hi = 0;
            this.flags = 0;
        }

        public Decimal(long value)
        {
            this.hi = 0;
            ulong magnitude;
            if (value < 0)
            {
                magnitude = (ulong)(-(value + 1)) + 1UL;
                this.flags = unchecked((int)0x80000000);
            }
            else
            {
                magnitude = (ulong)value;
                this.flags = 0;
            }
            this.lo = (int)(uint)magnitude;
            this.mid = (int)(uint)(magnitude >> 32);
        }

        public Decimal(ulong value)
        {
            this.lo = (int)(uint)value;
            this.mid = (int)(uint)(value >> 32);
            this.hi = 0;
            this.flags = 0;
        }

        public Decimal(double value)
        {
            this = FromDouble(value);
        }

        private int Scale { get { return (flags >> 16) & 0xFF; } }

        private bool IsNegative { get { return (flags & unchecked((int)0x80000000)) != 0; } }

        private bool IsZero { get { return lo == 0 && mid == 0 && hi == 0; } }

        [Lamella.Runtime.RuntimeProvided] public static Decimal operator +(Decimal d1, Decimal d2) { return default(Decimal); }
        [Lamella.Runtime.RuntimeProvided] public static Decimal operator -(Decimal d1, Decimal d2) { return default(Decimal); }
        [Lamella.Runtime.RuntimeProvided] public static Decimal operator *(Decimal d1, Decimal d2) { return default(Decimal); }
        [Lamella.Runtime.RuntimeProvided] public static Decimal operator /(Decimal d1, Decimal d2) { return default(Decimal); }
        [Lamella.Runtime.RuntimeProvided] public static Decimal operator %(Decimal d1, Decimal d2) { return default(Decimal); }

        [Lamella.Runtime.RuntimeProvided] public static int Compare(Decimal d1, Decimal d2) { return 0; }

        [Lamella.Runtime.RuntimeProvided] private static Decimal FromDouble(double value) { return default(Decimal); }

        [Lamella.Runtime.RuntimeProvided] private static double ToDouble(Decimal value) { return 0.0; }

        public static Decimal Add(Decimal d1, Decimal d2) { return d1 + d2; }
        public static Decimal Subtract(Decimal d1, Decimal d2) { return d1 - d2; }
        public static Decimal Multiply(Decimal d1, Decimal d2) { return d1 * d2; }
        public static Decimal Divide(Decimal d1, Decimal d2) { return d1 / d2; }
        public static Decimal Remainder(Decimal d1, Decimal d2) { return d1 % d2; }
        public static Decimal Negate(Decimal d) { return Zero - d; }

        public static Decimal operator -(Decimal d) { return Zero - d; }
        public static Decimal operator +(Decimal d) { return d; }

        public static bool operator ==(Decimal d1, Decimal d2) { return Compare(d1, d2) == 0; }
        public static bool operator !=(Decimal d1, Decimal d2) { return Compare(d1, d2) != 0; }
        public static bool operator <(Decimal d1, Decimal d2) { return Compare(d1, d2) < 0; }
        public static bool operator >(Decimal d1, Decimal d2) { return Compare(d1, d2) > 0; }
        public static bool operator <=(Decimal d1, Decimal d2) { return Compare(d1, d2) <= 0; }
        public static bool operator >=(Decimal d1, Decimal d2) { return Compare(d1, d2) >= 0; }

        public int CompareTo(Decimal value) { return Compare(this, value); }

        public int CompareTo(object value)
        {
            if (value == null) return 1;
            if (!(value is Decimal)) throw new ArgumentException("Object must be of type Decimal.");
            return Compare(this, (Decimal)value);
        }

        public bool Equals(Decimal value) { return Compare(this, value) == 0; }

        public override bool Equals(object value)
        {
            if (!(value is Decimal)) return false;
            return Compare(this, (Decimal)value) == 0;
        }

        public override int GetHashCode()
        {
            uint nlo = (uint)lo, nmid = (uint)mid, nhi = (uint)hi;
            int scale = Scale;
            while (scale > 0)
            {
                uint remainder;
                uint tlo, tmid, thi;
                DivBy10(nlo, nmid, nhi, out tlo, out tmid, out thi, out remainder);
                if (remainder != 0) break;
                nlo = tlo; nmid = tmid; nhi = thi; scale--;
            }
            return (int)(nlo ^ nmid ^ nhi);
        }

        public static implicit operator Decimal(int value) { return new Decimal(value); }
        public static implicit operator Decimal(uint value) { return new Decimal(value); }
        public static implicit operator Decimal(long value) { return new Decimal(value); }
        public static implicit operator Decimal(ulong value) { return new Decimal(value); }
        public static implicit operator Decimal(byte value) { return new Decimal((int)value); }
        public static implicit operator Decimal(short value) { return new Decimal((int)value); }

        public static explicit operator Decimal(double value) { return new Decimal(value); }

        public static explicit operator double(Decimal value) { return ToDouble(value); }

        public static explicit operator long(Decimal value)
        {
            ulong magnitude = value.IntegerMagnitude();
            if (value.IsNegative)
            {
                if (magnitude > 9223372036854775808UL) throw new OverflowException("Value was either too large or too small for an Int64.");
                if (magnitude == 9223372036854775808UL) return long.MinValue;
                return -(long)magnitude;
            }
            if (magnitude > 9223372036854775807UL) throw new OverflowException("Value was either too large or too small for an Int64.");
            return (long)magnitude;
        }

        public static explicit operator int(Decimal value)
        {
            long asLong = (long)value;
            if (asLong < -2147483648L || asLong > 2147483647L) throw new OverflowException("Value was either too large or too small for an Int32.");
            return (int)asLong;
        }

        private ulong IntegerMagnitude()
        {
            uint clo = (uint)lo, cmid = (uint)mid, chi = (uint)hi;
            int scale = Scale;
            for (int i = 0; i < scale; i++)
            {
                uint remainder;
                uint tlo, tmid, thi;
                DivBy10(clo, cmid, chi, out tlo, out tmid, out thi, out remainder);
                clo = tlo; cmid = tmid; chi = thi;
            }
            if (chi != 0) throw new OverflowException("Value was either too large or too small for an Int64.");
            return ((ulong)cmid << 32) | clo;
        }

        private static void DivBy10(uint clo, uint cmid, uint chi, out uint qlo, out uint qmid, out uint qhi, out uint remainder)
        {
            ulong cur = chi;
            qhi = (uint)(cur / 10);
            ulong rem = cur % 10;
            cur = (rem << 32) | cmid;
            qmid = (uint)(cur / 10);
            rem = cur % 10;
            cur = (rem << 32) | clo;
            qlo = (uint)(cur / 10);
            remainder = (uint)(cur % 10);
        }

        public override string ToString()
        {
            char[] digits = new char[32];
            int count = 0;
            uint clo = (uint)lo, cmid = (uint)mid, chi = (uint)hi;
            if (clo == 0 && cmid == 0 && chi == 0)
            {
                digits[count++] = '0';
            }
            else
            {
                while (clo != 0 || cmid != 0 || chi != 0)
                {
                    uint remainder;
                    uint tlo, tmid, thi;
                    DivBy10(clo, cmid, chi, out tlo, out tmid, out thi, out remainder);
                    digits[count++] = (char)('0' + (int)remainder);
                    clo = tlo; cmid = tmid; chi = thi;
                }
            }

            int scale = Scale;
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (IsNegative && !IsZero) result.Append('-');

            int intDigitCount = count - scale;
            if (intDigitCount <= 0)
            {
                result.Append('0');
            }
            else
            {
                for (int i = count - 1; i >= scale; i--) result.Append(digits[i]);
            }
            if (scale > 0)
            {
                result.Append('.');
                for (int i = scale - 1; i >= 0; i--)
                {
                    if (i < count) result.Append(digits[i]);
                    else result.Append('0');
                }
            }
            return result.ToString();
        }

        public string ToString(string format)
        {
            if ((object)format == null || format.Length == 0) return ToString();
            if (format.Length == 1 && (format[0] == 'G' || format[0] == 'g')) return ToString();
            throw new FormatException("Format specifier was invalid.");
        }

        public string ToString(string format, IFormatProvider provider)
        {
            return ToString(format);
        }

        public static Decimal Parse(string s)
        {
            Decimal result;
            int outcome = TryParseCore(s, out result);
            if (outcome == 1) return result;
            if (outcome == -2) throw new OverflowException("Value was either too large or too small for a Decimal.");
            if ((object)s == null) throw new ArgumentNullException("s");
            throw new FormatException("Input string was not in a correct format.");
        }

        public static bool TryParse(string s, out Decimal result)
        {
            return TryParseCore(s, out result) == 1;
        }

        private static int TryParseCore(string s, out Decimal result)
        {
            result = Zero;
            if ((object)s == null) return 0;
            int end = s.Length;
            while (end > 0 && Char.IsWhiteSpace(s[end - 1])) end = end - 1;
            int i = 0;
            while (i < end && Char.IsWhiteSpace(s[i])) i = i + 1;
            if (i >= end) return 0;

            bool negative = false;
            if (s[i] == '-') { negative = true; i = i + 1; }
            else if (s[i] == '+') { i = i + 1; }

            uint mlo = 0, mmid = 0, mhi = 0;
            int scale = 0;
            bool sawDigit = false;
            bool sawDot = false;
            while (i < end)
            {
                char c = s[i];
                if (c == '.')
                {
                    if (sawDot) return 0;
                    sawDot = true;
                    i = i + 1;
                    continue;
                }
                if (c < '0' || c > '9') return 0;
                sawDigit = true;
                int digit = c - '0';
                if (!MulAdd(ref mlo, ref mmid, ref mhi, (uint)digit)) return -2;
                if (sawDot)
                {
                    scale++;
                    if (scale > 28) return -2;
                }
                i = i + 1;
            }
            if (!sawDigit) return 0;

            result = new Decimal((int)mlo, (int)mmid, (int)mhi, negative, (byte)scale);
            return 1;
        }

        private static bool MulAdd(ref uint clo, ref uint cmid, ref uint chi, uint digit)
        {
            ulong cur = (ulong)clo * 10UL + digit;
            clo = (uint)cur;
            ulong carry = cur >> 32;
            cur = (ulong)cmid * 10UL + carry;
            cmid = (uint)cur;
            carry = cur >> 32;
            cur = (ulong)chi * 10UL + carry;
            chi = (uint)cur;
            carry = cur >> 32;
            return carry == 0;
        }
    }
}
