// Lamella managed corlib (from scratch). -- System.ValueTuple<...>
#if LAMELLA_SURFACE_TUPLES
namespace System
{

    /// <summary>A tuple of one element.</summary>
    public struct ValueTuple<T1>
    {
        /// <summary>The tuple's first element.</summary>
        public T1 Item1;

        /// <summary>Initializes a tuple holding the given element.</summary>
        public ValueTuple(T1 item1)
        {
            Item1 = item1;
        }

        /// <summary>Returns the tuple's elements in parentheses, separated by commas.</summary>
        public override string ToString()
        {
            return "(" + Item1 + ")";
        }
    }

    /// <summary>A tuple of two elements.</summary>
    public struct ValueTuple<T1, T2>
    {
        /// <summary>The tuple's first element.</summary>
        public T1 Item1;
        /// <summary>The tuple's second element.</summary>
        public T2 Item2;

        /// <summary>Initializes a tuple holding the given elements.</summary>
        public ValueTuple(T1 item1, T2 item2)
        {
            Item1 = item1;
            Item2 = item2;
        }

        /// <summary>Returns the tuple's elements in parentheses, separated by commas.</summary>
        public override string ToString()
        {
            return "(" + Item1 + ", " + Item2 + ")";
        }
    }

    /// <summary>A tuple of three elements.</summary>
    public struct ValueTuple<T1, T2, T3>
    {
        /// <summary>The tuple's first element.</summary>
        public T1 Item1;
        /// <summary>The tuple's second element.</summary>
        public T2 Item2;
        /// <summary>The tuple's third element.</summary>
        public T3 Item3;

        /// <summary>Initializes a tuple holding the given elements.</summary>
        public ValueTuple(T1 item1, T2 item2, T3 item3)
        {
            Item1 = item1;
            Item2 = item2;
            Item3 = item3;
        }

        /// <summary>Returns the tuple's elements in parentheses, separated by commas.</summary>
        public override string ToString()
        {
            return "(" + Item1 + ", " + Item2 + ", " + Item3 + ")";
        }
    }

    /// <summary>A tuple of four elements.</summary>
    public struct ValueTuple<T1, T2, T3, T4>
    {
        /// <summary>The tuple's first element.</summary>
        public T1 Item1;
        /// <summary>The tuple's second element.</summary>
        public T2 Item2;
        /// <summary>The tuple's third element.</summary>
        public T3 Item3;
        /// <summary>The tuple's fourth element.</summary>
        public T4 Item4;

        /// <summary>Initializes a tuple holding the given elements.</summary>
        public ValueTuple(T1 item1, T2 item2, T3 item3, T4 item4)
        {
            Item1 = item1;
            Item2 = item2;
            Item3 = item3;
            Item4 = item4;
        }

        /// <summary>Returns the tuple's elements in parentheses, separated by commas.</summary>
        public override string ToString()
        {
            return "(" + Item1 + ", " + Item2 + ", " + Item3 + ", " + Item4 + ")";
        }
    }

    /// <summary>A tuple of five elements.</summary>
    public struct ValueTuple<T1, T2, T3, T4, T5>
    {
        /// <summary>The tuple's first element.</summary>
        public T1 Item1;
        /// <summary>The tuple's second element.</summary>
        public T2 Item2;
        /// <summary>The tuple's third element.</summary>
        public T3 Item3;
        /// <summary>The tuple's fourth element.</summary>
        public T4 Item4;
        /// <summary>The tuple's fifth element.</summary>
        public T5 Item5;

        /// <summary>Initializes a tuple holding the given elements.</summary>
        public ValueTuple(T1 item1, T2 item2, T3 item3, T4 item4, T5 item5)
        {
            Item1 = item1;
            Item2 = item2;
            Item3 = item3;
            Item4 = item4;
            Item5 = item5;
        }

        /// <summary>Returns the tuple's elements in parentheses, separated by commas.</summary>
        public override string ToString()
        {
            return "(" + Item1 + ", " + Item2 + ", " + Item3 + ", " + Item4 + ", " + Item5 + ")";
        }
    }

    /// <summary>A tuple of six elements.</summary>
    public struct ValueTuple<T1, T2, T3, T4, T5, T6>
    {
        /// <summary>The tuple's first element.</summary>
        public T1 Item1;
        /// <summary>The tuple's second element.</summary>
        public T2 Item2;
        /// <summary>The tuple's third element.</summary>
        public T3 Item3;
        /// <summary>The tuple's fourth element.</summary>
        public T4 Item4;
        /// <summary>The tuple's fifth element.</summary>
        public T5 Item5;
        /// <summary>The tuple's sixth element.</summary>
        public T6 Item6;

        /// <summary>Initializes a tuple holding the given elements.</summary>
        public ValueTuple(T1 item1, T2 item2, T3 item3, T4 item4, T5 item5, T6 item6)
        {
            Item1 = item1;
            Item2 = item2;
            Item3 = item3;
            Item4 = item4;
            Item5 = item5;
            Item6 = item6;
        }

        /// <summary>Returns the tuple's elements in parentheses, separated by commas.</summary>
        public override string ToString()
        {
            return "(" + Item1 + ", " + Item2 + ", " + Item3 + ", " + Item4 + ", " + Item5
                + ", " + Item6 + ")";
        }
    }

    /// <summary>A tuple of seven elements.</summary>
    public struct ValueTuple<T1, T2, T3, T4, T5, T6, T7>
    {
        /// <summary>The tuple's first element.</summary>
        public T1 Item1;
        /// <summary>The tuple's second element.</summary>
        public T2 Item2;
        /// <summary>The tuple's third element.</summary>
        public T3 Item3;
        /// <summary>The tuple's fourth element.</summary>
        public T4 Item4;
        /// <summary>The tuple's fifth element.</summary>
        public T5 Item5;
        /// <summary>The tuple's sixth element.</summary>
        public T6 Item6;
        /// <summary>The tuple's seventh element.</summary>
        public T7 Item7;

        /// <summary>Initializes a tuple holding the given elements.</summary>
        public ValueTuple(T1 item1, T2 item2, T3 item3, T4 item4, T5 item5, T6 item6, T7 item7)
        {
            Item1 = item1;
            Item2 = item2;
            Item3 = item3;
            Item4 = item4;
            Item5 = item5;
            Item6 = item6;
            Item7 = item7;
        }

        /// <summary>Returns the tuple's elements in parentheses, separated by commas.</summary>
        public override string ToString()
        {
            return "(" + Item1 + ", " + Item2 + ", " + Item3 + ", " + Item4 + ", " + Item5
                + ", " + Item6 + ", " + Item7 + ")";
        }
    }

    /// <summary>A tuple of seven elements and a nested tuple holding the rest.</summary>
    public struct ValueTuple<T1, T2, T3, T4, T5, T6, T7, TRest> where TRest : struct
    {
        /// <summary>The tuple's first element.</summary>
        public T1 Item1;
        /// <summary>The tuple's second element.</summary>
        public T2 Item2;
        /// <summary>The tuple's third element.</summary>
        public T3 Item3;
        /// <summary>The tuple's fourth element.</summary>
        public T4 Item4;
        /// <summary>The tuple's fifth element.</summary>
        public T5 Item5;
        /// <summary>The tuple's sixth element.</summary>
        public T6 Item6;
        /// <summary>The tuple's seventh element.</summary>
        public T7 Item7;
        /// <summary>A tuple holding the elements past the seventh.</summary>
        public TRest Rest;

        /// <summary>Initializes a tuple holding the given elements and nested rest.</summary>
        public ValueTuple(T1 item1, T2 item2, T3 item3, T4 item4, T5 item5, T6 item6, T7 item7,
            TRest rest)
        {
            Item1 = item1;
            Item2 = item2;
            Item3 = item3;
            Item4 = item4;
            Item5 = item5;
            Item6 = item6;
            Item7 = item7;
            Rest = rest;
        }

        /// <summary>Returns every element in parentheses, separated by commas, with the nested
        /// tuple's elements spliced in rather than nested.</summary>
        public override string ToString()
        {
            string rest = Rest.ToString();
            if (rest.Length >= 2 && rest[0] == '(' && rest[rest.Length - 1] == ')')
            {
                rest = rest.Substring(1, rest.Length - 2);
            }
            return "(" + Item1 + ", " + Item2 + ", " + Item3 + ", " + Item4 + ", " + Item5
                + ", " + Item6 + ", " + Item7 + ", " + rest + ")";
        }
    }
}
#endif
