// Lamella managed corlib (from scratch). -- System.Span<T> and System.ReadOnlySpan<T>
#if LAMELLA_SURFACE_SPAN
namespace System
{
    public readonly ref struct Span<T>
    {
        private readonly T[] _items;
        private readonly int _start;
        private readonly int _length;

        /// <summary>Creates a span over the whole of <paramref name="array"/>.</summary>
        public Span(T[] array)
        {
            if (array == null)
            {
                _items = null;
                _start = 0;
                _length = 0;
                return;
            }
            _items = array;
            _start = 0;
            _length = array.Length;
        }

        /// <summary>Creates a span over <paramref name="length"/> elements of
        /// <paramref name="array"/> beginning at <paramref name="start"/>.</summary>
        public Span(T[] array, int start, int length)
        {
            if (array == null)
            {
                if (start != 0 || length != 0)
                {
                    throw new ArgumentOutOfRangeException();
                }
                _items = null;
                _start = 0;
                _length = 0;
                return;
            }
            if (start < 0 || length < 0 || start + length > array.Length)
            {
                throw new ArgumentOutOfRangeException();
            }
            _items = array;
            _start = start;
            _length = length;
        }

        /// <summary>The number of elements in this span.</summary>
        public int Length { get { return _length; } }

        /// <summary>Whether this span is empty.</summary>
        public bool IsEmpty { get { return _length == 0; } }

        /// <summary>The element at <paramref name="index"/>, by reference.</summary>
        public ref T this[int index]
        {
            get
            {
                if (index < 0 || index >= _length)
                {
                    throw new IndexOutOfRangeException();
                }
                return ref _items[_start + index];
            }
        }

        /// <summary>The rest of this span from <paramref name="start"/>.</summary>
        public Span<T> Slice(int start)
        {
            if (start < 0 || start > _length)
            {
                throw new ArgumentOutOfRangeException();
            }
            return new Span<T>(_items, _start + start, _length - start);
        }

        /// <summary><paramref name="length"/> elements of this span beginning at
        /// <paramref name="start"/>.</summary>
        public Span<T> Slice(int start, int length)
        {
            if (start < 0 || length < 0 || start + length > _length)
            {
                throw new ArgumentOutOfRangeException();
            }
            return new Span<T>(_items, _start + start, length);
        }

        /// <summary>Copies this span into a new array.</summary>
        public T[] ToArray()
        {
            T[] copy = new T[_length];
            for (int i = 0; i < _length; i++)
            {
                copy[i] = _items[_start + i];
            }
            return copy;
        }

        /// <summary>Copies this span into <paramref name="destination"/>.</summary>
        public void CopyTo(Span<T> destination)
        {
            if (destination.Length < _length)
            {
                throw new ArgumentException();
            }
            for (int i = 0; i < _length; i++)
            {
                destination[i] = _items[_start + i];
            }
        }

        /// <summary>Sets every element of this span to <paramref name="value"/>.</summary>
        public void Fill(T value)
        {
            for (int i = 0; i < _length; i++)
            {
                _items[_start + i] = value;
            }
        }

        /// <summary>Sets every element of this span to the default value of
        /// <typeparamref name="T"/>.</summary>
        public void Clear()
        {
            T zero = default(T);
            for (int i = 0; i < _length; i++)
            {
                _items[_start + i] = zero;
            }
        }
    }

    public readonly ref struct ReadOnlySpan<T>
    {
        private readonly T[] _items;
        private readonly int _start;
        private readonly int _length;

        /// <summary>Creates a read-only span over the whole of <paramref name="array"/>.</summary>
        public ReadOnlySpan(T[] array)
        {
            if (array == null)
            {
                _items = null;
                _start = 0;
                _length = 0;
                return;
            }
            _items = array;
            _start = 0;
            _length = array.Length;
        }

        /// <summary>Creates a read-only span over <paramref name="length"/> elements of
        /// <paramref name="array"/> beginning at <paramref name="start"/>.</summary>
        public ReadOnlySpan(T[] array, int start, int length)
        {
            if (array == null)
            {
                if (start != 0 || length != 0)
                {
                    throw new ArgumentOutOfRangeException();
                }
                _items = null;
                _start = 0;
                _length = 0;
                return;
            }
            if (start < 0 || length < 0 || start + length > array.Length)
            {
                throw new ArgumentOutOfRangeException();
            }
            _items = array;
            _start = start;
            _length = length;
        }

        /// <summary>The number of elements in this span.</summary>
        public int Length { get { return _length; } }

        /// <summary>Whether this span is empty.</summary>
        public bool IsEmpty { get { return _length == 0; } }

        /// <summary>The element at <paramref name="index"/>, by read-only reference.</summary>
        public ref readonly T this[int index]
        {
            get
            {
                if (index < 0 || index >= _length)
                {
                    throw new IndexOutOfRangeException();
                }
                return ref _items[_start + index];
            }
        }

        /// <summary>The rest of this span from <paramref name="start"/>.</summary>
        public ReadOnlySpan<T> Slice(int start)
        {
            if (start < 0 || start > _length)
            {
                throw new ArgumentOutOfRangeException();
            }
            return new ReadOnlySpan<T>(_items, _start + start, _length - start);
        }

        /// <summary><paramref name="length"/> elements of this span beginning at
        /// <paramref name="start"/>.</summary>
        public ReadOnlySpan<T> Slice(int start, int length)
        {
            if (start < 0 || length < 0 || start + length > _length)
            {
                throw new ArgumentOutOfRangeException();
            }
            return new ReadOnlySpan<T>(_items, _start + start, length);
        }

        /// <summary>Copies this span into a new array.</summary>
        public T[] ToArray()
        {
            T[] copy = new T[_length];
            for (int i = 0; i < _length; i++)
            {
                copy[i] = _items[_start + i];
            }
            return copy;
        }

        /// <summary>Copies this span into <paramref name="destination"/>.</summary>
        public void CopyTo(Span<T> destination)
        {
            if (destination.Length < _length)
            {
                throw new ArgumentException();
            }
            for (int i = 0; i < _length; i++)
            {
                destination[i] = _items[_start + i];
            }
        }
    }
}
#endif
