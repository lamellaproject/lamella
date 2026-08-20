// Lamella managed corlib (from scratch). -- System.Nullable<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{

    /// A value of type `T`, or the absence of one. `HasValue` distinguishes the two cases, and
    /// `Value` retrieves the contained value when there is one.
    ///
    /// Boxing converts an instance holding no value to a null reference, and an instance holding a
    /// value to a boxed `T` -- never to a boxed `Nullable<T>`.
    public struct Nullable<T> where T : struct
    {
        private bool hasValue;
        private T value;

        /// Creates an instance holding `value`. `HasValue` is then `true`.
        public Nullable(T value)
        {
            this.value = value;
            this.hasValue = true;
        }

        /// Whether this instance holds a value.
        public bool HasValue
        {
            get { return hasValue; }
        }

        /// The contained value. Throws `InvalidOperationException` when there is none.
        public T Value
        {
            get
            {
                if (!hasValue)
                {
                    throw new InvalidOperationException("Nullable object must have a value.");
                }
                return value;
            }
        }

        /// The contained value, or `T`'s default value when there is none.
        public T GetValueOrDefault()
        {
            return value;
        }

        /// The contained value, or `defaultValue` when there is none.
        public T GetValueOrDefault(T defaultValue)
        {
            if (!hasValue)
            {
                return defaultValue;
            }
            return value;
        }

        /// Whether `other` equals this instance: when there is no value, `true` only for a null
        /// reference; otherwise whether the contained value equals `other`.
        public override bool Equals(object other)
        {
            if (!hasValue)
            {
                return other == null;
            }
            if (other == null)
            {
                return false;
            }
            return value.Equals(other);
        }

        /// The contained value's hash code, or 0 when there is no value.
        public override int GetHashCode()
        {
            if (!hasValue)
            {
                return 0;
            }
            return value.GetHashCode();
        }

        /// The contained value's text, or the empty string when there is no value.
        public override string ToString()
        {
            if (!hasValue)
            {
                return "";
            }
            return value.ToString();
        }
    }
}
#endif
