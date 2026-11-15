// Lamella managed corlib (from scratch). -- System.Decimal
#if LAMELLA_SURFACE_DECIMAL
using System.Globalization;

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
            if (scale > 28) throw new ArgumentOutOfRangeException("scale", scale, "scale ('" + Int32.Decimal(scale) + "') must be less than or equal to '28'.");
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

#if LAMELLA_SURFACE_FLOAT
        public Decimal(double value)
        {
            this = FromDouble(value);
        }
#endif

        private int Scale { get { return (flags >> 16) & 0xFF; } }

        private bool IsNegative { get { return (flags & unchecked((int)0x80000000)) != 0; } }

        private bool IsZero { get { return lo == 0 && mid == 0 && hi == 0; } }

        [Lamella.Runtime.RuntimeProvided] private static Decimal DecAdd(Decimal d1, Decimal d2) { return new Decimal(); }
        public static Decimal operator +(Decimal d1, Decimal d2) { return DecAdd(d1, d2); }
        [Lamella.Runtime.RuntimeProvided] private static Decimal DecSub(Decimal d1, Decimal d2) { return new Decimal(); }
        public static Decimal operator -(Decimal d1, Decimal d2) { return DecSub(d1, d2); }
        [Lamella.Runtime.RuntimeProvided] private static Decimal DecMul(Decimal d1, Decimal d2) { return new Decimal(); }
        public static Decimal operator *(Decimal d1, Decimal d2) { return DecMul(d1, d2); }
        [Lamella.Runtime.RuntimeProvided] private static Decimal DecDiv(Decimal d1, Decimal d2) { return new Decimal(); }
        public static Decimal operator /(Decimal d1, Decimal d2) { return DecDiv(d1, d2); }
        [Lamella.Runtime.RuntimeProvided] private static Decimal DecRem(Decimal d1, Decimal d2) { return new Decimal(); }
        public static Decimal operator %(Decimal d1, Decimal d2) { return DecRem(d1, d2); }

        [Lamella.Runtime.RuntimeProvided] public static int Compare(Decimal d1, Decimal d2) { return 0; }

#if LAMELLA_SURFACE_FLOAT
        [Lamella.Runtime.RuntimeProvided] private static Decimal FromDouble(double value) { return new Decimal(); }

        [Lamella.Runtime.RuntimeProvided] private static double ToDouble(Decimal value) { return 0.0; }
#endif

        public static Decimal Add(Decimal d1, Decimal d2) { return d1 + d2; }
        public static Decimal Subtract(Decimal d1, Decimal d2) { return d1 - d2; }
        public static Decimal Multiply(Decimal d1, Decimal d2) { return d1 * d2; }
        public static Decimal Divide(Decimal d1, Decimal d2) { return d1 / d2; }
        public static Decimal Remainder(Decimal d1, Decimal d2) { return d1 % d2; }
        public static Decimal Negate(Decimal d) { return Zero - d; }

        public static Decimal operator -(Decimal d) { return Zero - d; }
        public static Decimal operator +(Decimal d) { return d; }

        public static Decimal operator ++(Decimal d) { return d + One; }
        public static Decimal operator --(Decimal d) { return d - One; }

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

#if LAMELLA_SURFACE_FLOAT
        public static explicit operator Decimal(double value) { return new Decimal(value); }

        public static explicit operator double(Decimal value) { return ToDouble(value); }
#endif

        private static long ToInt64(Decimal value)
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

        public static explicit operator long(Decimal value) { return ToInt64(value); }

        public static explicit operator int(Decimal value)
        {
            long asLong = ToInt64(value);
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


        private static void Shift(ref uint clo, ref uint cmid, ref uint chi, int places,
                                  out uint lastDropped, out bool restNonZero)
        {
            lastDropped = 0;
            restNonZero = false;
            for (int i = 0; i < places; i++)
            {
                if (lastDropped != 0) restNonZero = true;
                uint remainder;
                uint tlo, tmid, thi;
                DivBy10(clo, cmid, chi, out tlo, out tmid, out thi, out remainder);
                clo = tlo; cmid = tmid; chi = thi;
                lastDropped = remainder;
            }
        }

        private static void Increment(ref uint clo, ref uint cmid, ref uint chi)
        {
            clo = clo + 1;
            if (clo != 0) return;
            cmid = cmid + 1;
            if (cmid != 0) return;
            chi = chi + 1;
        }

        private static Decimal Compose(uint clo, uint cmid, uint chi, bool negative, int scale)
        {
            return new Decimal(unchecked((int)clo), unchecked((int)cmid), unchecked((int)chi), negative, (byte)scale);
        }

        public static Decimal Round(Decimal d, int decimals)
        {
            if (decimals < 0 || decimals > 28) throw new ArgumentOutOfRangeException("decimals", "Decimal can only round to between 0 and 28 digits of precision.");
            int scale = d.Scale;
            if (scale <= decimals) return d;
            uint clo = (uint)d.lo, cmid = (uint)d.mid, chi = (uint)d.hi;
            uint lastDropped;
            bool restNonZero;
            Shift(ref clo, ref cmid, ref chi, scale - decimals, out lastDropped, out restNonZero);
            bool roundUp = lastDropped > 5
                || (lastDropped == 5 && (restNonZero || (clo & 1) == 1));
            if (roundUp) Increment(ref clo, ref cmid, ref chi);
            bool negative = d.IsNegative && !(clo == 0 && cmid == 0 && chi == 0);
            return Compose(clo, cmid, chi, negative, decimals);
        }

        public static Decimal Truncate(Decimal d)
        {
            int scale = d.Scale;
            if (scale == 0) return d;
            uint clo = (uint)d.lo, cmid = (uint)d.mid, chi = (uint)d.hi;
            uint lastDropped;
            bool restNonZero;
            Shift(ref clo, ref cmid, ref chi, scale, out lastDropped, out restNonZero);
            bool negative = d.IsNegative && !(clo == 0 && cmid == 0 && chi == 0);
            return Compose(clo, cmid, chi, negative, 0);
        }

        public static Decimal Floor(Decimal d)
        {
            int scale = d.Scale;
            if (scale == 0) return d;
            uint clo = (uint)d.lo, cmid = (uint)d.mid, chi = (uint)d.hi;
            uint lastDropped;
            bool restNonZero;
            Shift(ref clo, ref cmid, ref chi, scale, out lastDropped, out restNonZero);
            if (d.IsNegative && (lastDropped != 0 || restNonZero)) Increment(ref clo, ref cmid, ref chi);
            bool negative = d.IsNegative && !(clo == 0 && cmid == 0 && chi == 0);
            return Compose(clo, cmid, chi, negative, 0);
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
            return NumberFormatter.FormatDecimal(ToString(), format);
        }

        public string ToString(IFormatProvider provider)
        {
            NumberParser.AskProvider(provider);
            return ToString();
        }

        public string ToString(string format, IFormatProvider provider)
        {
            NumberParser.AskProvider(provider);
            return ToString(format);
        }

        public static Decimal Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            Decimal result;
            int outcome = NumberParser.ScanDecimal(s, (int)NumberStyles.Number, out result);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a Decimal.");
            return result;
        }

        public static Decimal Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckFractionalStyle(style);
            Decimal result;
            int outcome = NumberParser.ScanDecimal(s, (int)style, out result);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a Decimal.");
            return result;
        }

        public static Decimal Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Number, provider);
        }

        public static Decimal Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckFractionalStyle(style);
            NumberParser.AskProvider(provider);
            Decimal result;
            int outcome = NumberParser.ScanDecimal(s, (int)style, out result);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a Decimal.");
            return result;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out Decimal result)
        {
            result = Zero;
            if ((object)s == null) return false;
            Decimal scanned;
            if (NumberParser.ScanDecimal(s, (int)NumberStyles.Number, out scanned) != NumberParser.Done) return false;
            result = scanned;
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out Decimal result)
        {
            NumberParser.CheckFractionalStyle(style);
            result = Zero;
            if ((object)s == null) return false;
            NumberParser.AskProvider(provider);
            Decimal scanned;
            if (NumberParser.ScanDecimal(s, (int)style, out scanned) != NumberParser.Done) return false;
            result = scanned;
            return true;
        }
#endif
    }
}
#endif
