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

        /// <summary>Initializes a decimal from its binary representation (ECMA-335 6th ed IV.5.7).</summary>
        /// <param name="bits">Four integers: the low, middle and high 32 bits of the 96-bit integer, then
        /// the scale (bits 16 to 23) and the sign (bit 31).</param>
        public Decimal(int[] bits)
        {
            if (bits == null) throw new ArgumentNullException("bits");
            if (bits.Length != 4 || !IsValidFlags(bits[3])) throw new ArgumentException("Decimal constructor requires an array or span of four valid decimal bytes.");
            this.lo = bits[0];
            this.mid = bits[1];
            this.hi = bits[2];
            this.flags = bits[3];
        }

        private static bool IsValidFlags(int flags)
        {
            return (flags & ~(unchecked((int)0x80000000) | 0x00FF0000)) == 0 && (flags & 0x00FF0000) <= (28 << 16);
        }

        /// <summary>Returns the binary representation of a decimal (ECMA-335 6th ed IV.5.7).</summary>
        /// <param name="d">The value.</param>
        /// <returns>A new array of four integers: the low, middle and high 32 bits of the 96-bit integer,
        /// then the scale (bits 16 to 23) and the sign (bit 31).</returns>
        public static int[] GetBits(Decimal d)
        {
            return new int[] { d.lo, d.mid, d.hi, d.flags };
        }

        /// <summary>Converts an OLE Automation Currency value, a count of ten-thousandths, to a decimal (.NET's
        /// member since .NET Framework 1.0; ECMA-335 6th ed IV.5.7 has none).</summary>
        /// <param name="cy">The Currency value.</param>
        /// <returns>The decimal <paramref name="cy"/> / 10,000.</returns>
        public static Decimal FromOACurrency(long cy)
        {
            ulong magnitude = cy < 0 ? (ulong)(-(cy + 1)) + 1UL : (ulong)cy;
            int scale = 4;
            if (magnitude != 0)
            {
                while (scale > 0 && magnitude % 10 == 0)
                {
                    magnitude = magnitude / 10;
                    scale--;
                }
            }
            return new Decimal((int)(uint)magnitude, (int)(uint)(magnitude >> 32), 0, cy < 0, (byte)scale);
        }

        /// <summary>Converts a decimal to an OLE Automation Currency value, a count of ten-thousandths (.NET's
        /// member since .NET Framework 1.0; ECMA-335 6th ed IV.5.7 has none).</summary>
        /// <param name="value">The value, rounded to four places, a tie to the even neighbor.</param>
        /// <returns>The Currency value.</returns>
        /// <exception cref="OverflowException">The rounded value is outside the Currency range.</exception>
        public static long ToOACurrency(Decimal value)
        {
            Decimal rounded = Round(value, 4);
            uint clo = (uint)rounded.lo, cmid = (uint)rounded.mid, chi = (uint)rounded.hi;
            for (int i = rounded.Scale; i < 4 && chi == 0; i++)
            {
                ulong t = (ulong)clo * 10;
                clo = (uint)t;
                t = (ulong)cmid * 10 + (t >> 32);
                cmid = (uint)t;
                chi = (uint)(t >> 32);
            }
            ulong magnitude = ((ulong)cmid << 32) | clo;
            if (chi == 0)
            {
                if (!rounded.IsNegative && magnitude <= 9223372036854775807UL) return (long)magnitude;
                if (rounded.IsNegative && magnitude <= 9223372036854775808UL)
                {
                    return magnitude == 9223372036854775808UL ? long.MinValue : -(long)magnitude;
                }
            }
            throw new OverflowException("Value was either too large or too small for a Currency.");
        }

        /// <summary>Returns whether two decimals have the same value.</summary>
        /// <param name="d1">The first value.</param>
        /// <param name="d2">The second value.</param>
        /// <returns>True when the values are equal, whatever their scales.</returns>
        public static bool Equals(Decimal d1, Decimal d2)
        {
            return Compare(d1, d2) == 0;
        }

#if LAMELLA_SURFACE_FLOAT
        public Decimal(double value)
        {
            this = FromDouble(value);
        }
#endif

        private int Scale { get { return (flags >> 16) & 0xFF; } }

        internal bool IsNegative { get { return (flags & unchecked((int)0x80000000)) != 0; } }

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

        /// <summary>Converts a decimal to the nearest double-precision value this runtime gives.</summary>
        /// <param name="d">The value to convert.</param>
        /// <returns>The converted value.</returns>
        [Lamella.Runtime.RuntimeProvided] public static double ToDouble(Decimal d) { return 0.0; }

        /// <summary>Converts a decimal to the nearest single-precision value.</summary>
        /// <param name="d">The value to convert.</param>
        /// <returns>The nearest <see cref="Single"/> to <paramref name="d"/>.</returns>
        public static float ToSingle(Decimal d)
        {
            uint nlo = (uint)d.lo, nmid = (uint)d.mid, nhi = (uint)d.hi;
            bool negative = d.IsNegative;
            if ((nlo | nmid | nhi) == 0) return BitConverter.Int32BitsToSingle(negative ? unchecked((int)0x80000000) : 0);
            uint dlo, dmid, dhi;
            PowerOfTen(d.Scale, out dlo, out dmid, out dhi);
            uint rlo = 0, rmid = 0, rhi = 0;
            int q = 0, count = 0, last = 0;
            bool sticky = false;
            for (int step = 0; ; step++)
            {
                uint inBit = 0;
                if (step < 96)
                {
                    int i = 95 - step;
                    inBit = i >= 64 ? (nhi >> (i - 64)) & 1 : (i >= 32 ? (nmid >> (i - 32)) & 1 : (nlo >> i) & 1);
                }
                rhi = (rhi << 1) | (rmid >> 31);
                rmid = (rmid << 1) | (rlo >> 31);
                rlo = (rlo << 1) | inBit;
                int bit = 0;
                if (rhi > dhi || (rhi == dhi && (rmid > dmid || (rmid == dmid && rlo >= dlo))))
                {
                    uint borrow = rlo < dlo ? 1u : 0u;
                    rlo = rlo - dlo;
                    uint borrow2 = ((ulong)rmid < (ulong)dmid + borrow) ? 1u : 0u;
                    rmid = rmid - dmid - borrow;
                    rhi = rhi - dhi - borrow2;
                    bit = 1;
                }
                if (count > 0 || bit == 1)
                {
                    if (count < 25) { q = (q << 1) | bit; count++; if (count == 25) last = step; }
                    else if (bit == 1) sticky = true;
                }
                if (count == 25 && step >= 95) break;
            }
            if ((rlo | rmid | rhi) != 0) sticky = true;
            int exponent = 95 - last + 1;
            bool round = (q & 1) == 1;
            q >>= 1;
            if (round && (sticky || (q & 1) == 1))
            {
                q++;
                if (q == 1 << 24) { q >>= 1; exponent++; }
            }
            int bits = ((exponent + 23 + 127) << 23) | (q & 0x7FFFFF);
            if (negative) bits |= unchecked((int)0x80000000);
            return BitConverter.Int32BitsToSingle(bits);
        }

        private static void PowerOfTen(int scale, out uint lo, out uint mid, out uint hi)
        {
            lo = 1; mid = 0; hi = 0;
            for (int i = 0; i < scale; i++)
            {
                ulong t = (ulong)lo * 10;
                lo = (uint)t;
                t = (ulong)mid * 10 + (t >> 32);
                mid = (uint)t;
                hi = (uint)((ulong)hi * 10 + (t >> 32));
            }
        }

        /// <summary>Initializes a decimal to the value of a single-precision number, rounded to the
        /// nearest decimal.</summary>
        /// <param name="value">The value to convert.</param>
        /// <exception cref="OverflowException"><paramref name="value"/> is NaN, infinite, or too large for a decimal.</exception>
        public Decimal(float value)
        {
            this = FromDouble((double)value);
        }

        public static explicit operator Decimal(float value) { return new Decimal(value); }
#endif

        public static Decimal Add(Decimal d1, Decimal d2) { return d1 + d2; }
        public static Decimal Subtract(Decimal d1, Decimal d2) { return d1 - d2; }
        public static Decimal Multiply(Decimal d1, Decimal d2) { return d1 * d2; }
        public static Decimal Divide(Decimal d1, Decimal d2) { return d1 / d2; }
        public static Decimal Remainder(Decimal d1, Decimal d2) { return d1 % d2; }
        public static Decimal Negate(Decimal d) { return -d; }

        public static Decimal operator -(Decimal d)
        {
            d.flags = d.flags ^ unchecked((int)0x80000000);
            return d;
        }
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
        public static implicit operator Decimal(sbyte value) { return new Decimal((int)value); }
        public static implicit operator Decimal(ushort value) { return new Decimal((int)value); }
        public static implicit operator Decimal(char value) { return new Decimal((int)value); }

#if LAMELLA_SURFACE_FLOAT
        public static explicit operator Decimal(double value) { return new Decimal(value); }

        public static explicit operator double(Decimal value) { return ToDouble(value); }

        public static explicit operator float(Decimal value) { return ToSingle(value); }
#endif


        /// <summary>Converts a decimal to an 8-bit unsigned integer, truncating toward zero.</summary>
        /// <param name="value">The value to convert.</param>
        /// <returns>The whole part of <paramref name="value"/>.</returns>
        /// <exception cref="OverflowException">The whole part is outside the range of <see cref="Byte"/>.</exception>
        public static byte ToByte(Decimal value)
        {
            ulong magnitude;
            if (WholeWithin(value, 255UL, 0UL, out magnitude)) return (byte)magnitude;
            throw new OverflowException("Value was either too large or too small for an unsigned byte.");
        }

        /// <summary>Converts a decimal to an 8-bit signed integer, truncating toward zero.</summary>
        /// <param name="value">The value to convert.</param>
        /// <returns>The whole part of <paramref name="value"/>.</returns>
        /// <exception cref="OverflowException">The whole part is outside the range of <see cref="SByte"/>.</exception>
        public static sbyte ToSByte(Decimal value)
        {
            ulong magnitude;
            if (WholeWithin(value, 127UL, 128UL, out magnitude)) return (sbyte)(value.IsNegative ? -(int)magnitude : (int)magnitude);
            throw new OverflowException("Value was either too large or too small for a signed byte.");
        }

        /// <summary>Converts a decimal to a 16-bit signed integer, truncating toward zero.</summary>
        /// <param name="value">The value to convert.</param>
        /// <returns>The whole part of <paramref name="value"/>.</returns>
        /// <exception cref="OverflowException">The whole part is outside the range of <see cref="Int16"/>.</exception>
        public static short ToInt16(Decimal value)
        {
            ulong magnitude;
            if (WholeWithin(value, 32767UL, 32768UL, out magnitude)) return (short)(value.IsNegative ? -(int)magnitude : (int)magnitude);
            throw new OverflowException("Value was either too large or too small for an Int16.");
        }

        /// <summary>Converts a decimal to a 16-bit unsigned integer, truncating toward zero.</summary>
        /// <param name="value">The value to convert.</param>
        /// <returns>The whole part of <paramref name="value"/>.</returns>
        /// <exception cref="OverflowException">The whole part is outside the range of <see cref="UInt16"/>.</exception>
        public static ushort ToUInt16(Decimal value)
        {
            ulong magnitude;
            if (WholeWithin(value, 65535UL, 0UL, out magnitude)) return (ushort)magnitude;
            throw new OverflowException("Value was either too large or too small for a UInt16.");
        }

        /// <summary>Converts a decimal to a 32-bit signed integer, truncating toward zero.</summary>
        /// <param name="d">The value to convert.</param>
        /// <returns>The whole part of <paramref name="d"/>.</returns>
        /// <exception cref="OverflowException">The whole part is outside the range of <see cref="Int32"/>.</exception>
        public static int ToInt32(Decimal d)
        {
            ulong magnitude;
            if (WholeWithin(d, 2147483647UL, 2147483648UL, out magnitude)) return (int)(d.IsNegative ? -(long)magnitude : (long)magnitude);
            throw new OverflowException("Value was either too large or too small for an Int32.");
        }

        /// <summary>Converts a decimal to a 32-bit unsigned integer, truncating toward zero.</summary>
        /// <param name="d">The value to convert.</param>
        /// <returns>The whole part of <paramref name="d"/>.</returns>
        /// <exception cref="OverflowException">The whole part is outside the range of <see cref="UInt32"/>.</exception>
        public static uint ToUInt32(Decimal d)
        {
            ulong magnitude;
            if (WholeWithin(d, 4294967295UL, 0UL, out magnitude)) return (uint)magnitude;
            throw new OverflowException("Value was either too large or too small for a UInt32.");
        }

        /// <summary>Converts a decimal to a 64-bit signed integer, truncating toward zero.</summary>
        /// <param name="d">The value to convert.</param>
        /// <returns>The whole part of <paramref name="d"/>.</returns>
        /// <exception cref="OverflowException">The whole part is outside the range of <see cref="Int64"/>.</exception>
        public static long ToInt64(Decimal d)
        {
            ulong magnitude;
            if (WholeWithin(d, 9223372036854775807UL, 9223372036854775808UL, out magnitude))
            {
                if (!d.IsNegative) return (long)magnitude;
                return magnitude == 9223372036854775808UL ? long.MinValue : -(long)magnitude;
            }
            throw new OverflowException("Value was either too large or too small for an Int64.");
        }

        /// <summary>Converts a decimal to a 64-bit unsigned integer, truncating toward zero.</summary>
        /// <param name="d">The value to convert.</param>
        /// <returns>The whole part of <paramref name="d"/>.</returns>
        /// <exception cref="OverflowException">The whole part is outside the range of <see cref="UInt64"/>.</exception>
        public static ulong ToUInt64(Decimal d)
        {
            ulong magnitude;
            if (WholeWithin(d, 18446744073709551615UL, 0UL, out magnitude)) return magnitude;
            throw new OverflowException("Value was either too large or too small for a UInt64.");
        }

        public static explicit operator byte(Decimal value) { return ToByte(value); }
        public static explicit operator sbyte(Decimal value) { return ToSByte(value); }
        public static explicit operator short(Decimal value) { return ToInt16(value); }
        public static explicit operator ushort(Decimal value) { return ToUInt16(value); }
        public static explicit operator int(Decimal value) { return ToInt32(value); }
        public static explicit operator uint(Decimal value) { return ToUInt32(value); }
        public static explicit operator long(Decimal value) { return ToInt64(value); }
        public static explicit operator ulong(Decimal value) { return ToUInt64(value); }

        public static explicit operator char(Decimal value)
        {
            ulong magnitude;
            if (WholeWithin(value, 65535UL, 0UL, out magnitude)) return (char)magnitude;
            throw new OverflowException("Value was either too large or too small for a character.",
                new OverflowException("Value was either too large or too small for a UInt16."));
        }

        private static bool WholeWithin(Decimal d, ulong positiveLimit, ulong negativeLimit, out ulong magnitude)
        {
            uint clo = (uint)d.lo, cmid = (uint)d.mid, chi = (uint)d.hi;
            int scale = d.Scale;
            for (int i = 0; i < scale; i++)
            {
                uint remainder;
                uint tlo, tmid, thi;
                DivBy10(clo, cmid, chi, out tlo, out tmid, out thi, out remainder);
                clo = tlo; cmid = tmid; chi = thi;
            }
            magnitude = ((ulong)cmid << 32) | clo;
            if (chi != 0) return false;
            return d.IsNegative ? magnitude <= negativeLimit : magnitude <= positiveLimit;
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
            bool negative = d.IsNegative;
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
            bool negative = d.IsNegative;
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
            bool negative = d.IsNegative;
            return Compose(clo, cmid, chi, negative, 0);
        }

#if LAMELLA_SURFACE_NETFX_2_0

        /// <summary>Returns the smallest whole number not less than a decimal.</summary>
        /// <param name="d">The value.</param>
        /// <returns>The value rounded toward positive infinity, at scale 0.</returns>
        public static Decimal Ceiling(Decimal d)
        {
            return RoundCore(d, 0, 4);
        }

        /// <summary>Rounds a decimal to a whole number, a tie going to the even neighbor.</summary>
        /// <param name="d">The value.</param>
        /// <returns>The nearest whole number.</returns>
        public static Decimal Round(Decimal d)
        {
            return Round(d, 0);
        }

        /// <summary>Rounds a decimal to a whole number, settling a tie by <paramref name="mode"/>.</summary>
        /// <param name="d">The value.</param>
        /// <param name="mode">How a value halfway between two whole numbers rounds.</param>
        /// <returns>The rounded value.</returns>
        public static Decimal Round(Decimal d, MidpointRounding mode)
        {
            return Round(d, 0, mode);
        }

        /// <summary>Rounds a decimal to a number of places, settling a tie by <paramref name="mode"/>.</summary>
        /// <param name="d">The value.</param>
        /// <param name="decimals">How many decimal places to keep, from 0 to 28.</param>
        /// <param name="mode">How a value halfway between two neighbors rounds.</param>
        /// <returns>The rounded value.</returns>
        public static Decimal Round(Decimal d, int decimals, MidpointRounding mode)
        {
            if (decimals < 0 || decimals > 28) throw new ArgumentOutOfRangeException("decimals", "Decimal can only round to between 0 and 28 digits of precision.");
            int code = (int)mode;
#if LAMELLA_SURFACE_NETCORE_3_0
            bool valid = code >= 0 && code <= 4;
#else
            bool valid = code == 0 || code == 1;
#endif
            if (!valid) throw new ArgumentException("The value '" + Int32.Decimal(code) + "' is not valid for this usage of the type MidpointRounding.", "mode");
            return RoundCore(d, decimals, code);
        }

        private static Decimal RoundCore(Decimal d, int decimals, int code)
        {
            int scale = d.Scale;
            if (scale <= decimals) return d;
            uint clo = (uint)d.lo, cmid = (uint)d.mid, chi = (uint)d.hi;
            uint lastDropped;
            bool restNonZero;
            Shift(ref clo, ref cmid, ref chi, scale - decimals, out lastDropped, out restNonZero);
            bool lost = lastDropped != 0 || restNonZero;
            bool up;
            if (code == 0) up = lastDropped > 5 || (lastDropped == 5 && (restNonZero || (clo & 1) == 1));
            else if (code == 1) up = lastDropped >= 5;
            else if (code == 2) up = false;
            else if (code == 3) up = d.IsNegative && lost;
            else up = !d.IsNegative && lost;
            if (up) Increment(ref clo, ref cmid, ref chi);
            bool negative = d.IsNegative;
            return Compose(clo, cmid, chi, negative, decimals);
        }
#endif

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
