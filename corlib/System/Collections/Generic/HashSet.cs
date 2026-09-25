// Lamella managed corlib (from scratch). -- System.Collections.Generic.HashSet<T>
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Collections.Generic
{
    /// <summary>A set of distinct elements, looked up through an equality comparer.</summary>
    /// <typeparam name="T">The type of the elements.</typeparam>
    public class HashSet<T> : ISet<T>, ICollection<T>, IEnumerable<T>, IEnumerable
    {
        private const int FreeSlot = -1;

        private int[] buckets;
        private int[] hashes;
        private int[] next;
        private T[] slots;
        private int shift;
        private int used;
        private int freeHead;
        private int freeCount;
        private int version;
        private IEqualityComparer<T> comparer;

        /// <summary>An empty set that compares elements with the default equality comparer for <typeparamref name="T"/>.</summary>
        public HashSet()
        {
            Initialize(null);
        }

        /// <summary>An empty set that compares elements with <paramref name="comparer"/>.</summary>
        /// <param name="comparer">The comparer, or null for the default comparer for <typeparamref name="T"/>.</param>
        public HashSet(IEqualityComparer<T> comparer)
        {
            Initialize(comparer);
        }

        /// <summary>A set holding the distinct elements of <paramref name="collection"/>, compared with the default comparer.</summary>
        /// <param name="collection">The elements to add.</param>
        /// <exception cref="ArgumentNullException"><paramref name="collection"/> is null.</exception>
        public HashSet(IEnumerable<T> collection)
        {
            if (collection == null) throw new ArgumentNullException("collection");
            Initialize(null);
            UnionWith(collection);
        }

        /// <summary>A set holding the distinct elements of <paramref name="collection"/>, compared with <paramref name="comparer"/>.</summary>
        /// <param name="collection">The elements to add.</param>
        /// <param name="comparer">The comparer, or null for the default comparer for <typeparamref name="T"/>.</param>
        /// <exception cref="ArgumentNullException"><paramref name="collection"/> is null.</exception>
        public HashSet(IEnumerable<T> collection, IEqualityComparer<T> comparer)
        {
            if (collection == null) throw new ArgumentNullException("collection");
            Initialize(comparer);
            UnionWith(collection);
        }

        private void Initialize(IEqualityComparer<T> comparer)
        {
            if (comparer == null) comparer = EqualityComparer<T>.Default;
            this.comparer = comparer;
        }

        /// <summary>The comparer that decides whether two elements are equal.</summary>
        public IEqualityComparer<T> Comparer
        {
            get { return comparer; }
        }

        /// <summary>How many elements the set holds.</summary>
        public int Count
        {
            get { return used - freeCount; }
        }

        bool ICollection<T>.IsReadOnly
        {
            get { return false; }
        }

        /// <summary>Adds <paramref name="item"/> unless an equal element is already present.</summary>
        /// <param name="item">The element to add.</param>
        /// <returns>True when the element was added; false when an equal one was already present.</returns>
        public bool Add(T item)
        {
            if (buckets == null) Allocate(0);
            int hash = HashOf(item);
            if (FindSlot(item, hash) >= 0) return false;
            Insert(item, hash);
            return true;
        }

        private int Insert(T item, int hash)
        {
            int slot;
            if (freeCount > 0)
            {
                slot = freeHead;
                freeHead = next[slot];
                freeCount = freeCount - 1;
            }
            else
            {
                if (used == hashes.Length) Grow();
                slot = used;
                used = used + 1;
            }
            int bucket = BucketOf(hash);
            hashes[slot] = hash;
            slots[slot] = item;
            next[slot] = buckets[bucket] - 1;
            buckets[bucket] = slot + 1;
            version = version + 1;
            return slot;
        }

        void ICollection<T>.Add(T item)
        {
            Add(item);
        }

        /// <summary>Removes every element.</summary>
        public void Clear()
        {
            if (used == 0) return;
            for (int bucket = 0; bucket < buckets.Length; bucket++)
            {
                buckets[bucket] = 0;
            }
            for (int slot = 0; slot < used; slot++)
            {
                hashes[slot] = FreeSlot;
                next[slot] = -1;
                slots[slot] = default(T);
            }
            used = 0;
            freeCount = 0;
        }

        /// <summary>Whether an element equal to <paramref name="item"/> is present.</summary>
        /// <param name="item">The element to look for.</param>
        /// <returns>True when a matching element is present.</returns>
        public bool Contains(T item)
        {
            if (buckets == null) return false;
            return FindSlot(item, HashOf(item)) >= 0;
        }

        /// <summary>Removes the element equal to <paramref name="item"/>.</summary>
        /// <param name="item">The element to remove.</param>
        /// <returns>True when a matching element was present and has been removed.</returns>
        public bool Remove(T item)
        {
            if (buckets == null) return false;
            int hash = HashOf(item);
            int bucket = BucketOf(hash);
            int previous = -1;
            int slot = buckets[bucket] - 1;
            while (slot >= 0)
            {
                if (hashes[slot] == hash && comparer.Equals(slots[slot], item))
                {
                    if (previous < 0)
                    {
                        buckets[bucket] = next[slot] + 1;
                    }
                    else
                    {
                        next[previous] = next[slot];
                    }
                    FreeSlotAt(slot);
                    return true;
                }
                previous = slot;
                slot = next[slot];
            }
            return false;
        }

        /// <summary>Removes every element that <paramref name="match"/> accepts.</summary>
        /// <param name="match">The test each element is put to.</param>
        /// <returns>How many elements were removed.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="match"/> is null.</exception>
        public int RemoveWhere(Predicate<T> match)
        {
            if (match == null) throw new ArgumentNullException("match");
            int removed = 0;
            for (int slot = 0; slot < used; slot++)
            {
                if (hashes[slot] < 0) continue;
                T item = slots[slot];
                if (match(item) && Remove(item)) removed = removed + 1;
            }
            return removed;
        }

        /// <summary>Copies every element into <paramref name="array"/>, from its start.</summary>
        /// <param name="array">The destination.</param>
        /// <exception cref="ArgumentNullException"><paramref name="array"/> is null.</exception>
        /// <exception cref="ArgumentException">The elements do not fit.</exception>
        public void CopyTo(T[] array)
        {
            CopyTo(array, 0, Count);
        }

        /// <summary>Copies every element into <paramref name="array"/>, starting at <paramref name="arrayIndex"/>.</summary>
        /// <param name="array">The destination.</param>
        /// <param name="arrayIndex">Where the first element goes.</param>
        /// <exception cref="ArgumentNullException"><paramref name="array"/> is null.</exception>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="arrayIndex"/> is negative.</exception>
        /// <exception cref="ArgumentException">The elements do not fit.</exception>
        public void CopyTo(T[] array, int arrayIndex)
        {
            CopyTo(array, arrayIndex, Count);
        }

        /// <summary>Copies up to <paramref name="count"/> elements into <paramref name="array"/>, starting at <paramref name="arrayIndex"/>.</summary>
        /// <param name="array">The destination.</param>
        /// <param name="arrayIndex">Where the first element goes.</param>
        /// <param name="count">The most elements to copy.</param>
        /// <exception cref="ArgumentNullException"><paramref name="array"/> is null.</exception>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="arrayIndex"/> or <paramref name="count"/> is negative.</exception>
        /// <exception cref="ArgumentException"><paramref name="count"/> elements do not fit after <paramref name="arrayIndex"/>.</exception>
        public void CopyTo(T[] array, int arrayIndex, int count)
        {
            if (array == null) throw new ArgumentNullException("array");
            if (arrayIndex < 0) throw new ArgumentOutOfRangeException("arrayIndex");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (arrayIndex > array.Length || count > array.Length - arrayIndex)
            {
                throw new ArgumentException("Destination array is not long enough to copy all the items in the collection. Check array index and length.");
            }
            int copied = 0;
            for (int slot = 0; slot < used && copied < count; slot++)
            {
                if (hashes[slot] < 0) continue;
                array[arrayIndex + copied] = slots[slot];
                copied = copied + 1;
            }
        }

        /// <summary>An enumerator over the elements.</summary>
        /// <returns>The enumerator.</returns>
        public Enumerator GetEnumerator()
        {
            return new Enumerator(this);
        }

        IEnumerator<T> IEnumerable<T>.GetEnumerator()
        {
            return new Enumerator(this);
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return new Enumerator(this);
        }

        /// <summary>Sets the room the set has to what its elements need.</summary>
        public void TrimExcess()
        {
            if (buckets == null) return;
            int count = Count;
            int size = 4;
            while (size < count) size = size * 2;
            if (size >= hashes.Length) return;
            T[] items = new T[count];
            int[] itemHashes = new int[count];
            int kept = 0;
            for (int slot = 0; slot < used; slot++)
            {
                if (hashes[slot] < 0) continue;
                items[kept] = slots[slot];
                itemHashes[kept] = hashes[slot];
                kept = kept + 1;
            }
            used = 0;
            freeCount = 0;
            Allocate(count);
            for (int i = 0; i < count; i++)
            {
                AddNew(items[i], itemHashes[i]);
            }
            version = version + 1;
        }

        // ---- the set operations ------------------------------------------------------------------

        /// <summary>Adds every element of <paramref name="other"/> that is not already present.</summary>
        /// <param name="other">The elements to add.</param>
        /// <exception cref="ArgumentNullException"><paramref name="other"/> is null.</exception>
        public void UnionWith(IEnumerable<T> other)
        {
            if (other == null) throw new ArgumentNullException("other");
            foreach (T item in other)
            {
                Add(item);
            }
        }

        /// <summary>Removes every element that <paramref name="other"/> holds.</summary>
        /// <param name="other">The elements to remove.</param>
        /// <exception cref="ArgumentNullException"><paramref name="other"/> is null.</exception>
        public void ExceptWith(IEnumerable<T> other)
        {
            if (other == null) throw new ArgumentNullException("other");
            if (Count == 0) return;
            if ((object)other == (object)this)
            {
                Clear();
                return;
            }
            foreach (T item in other)
            {
                Remove(item);
            }
        }

        /// <summary>Keeps only the elements that <paramref name="other"/> also holds.</summary>
        /// <param name="other">The elements to keep.</param>
        /// <exception cref="ArgumentNullException"><paramref name="other"/> is null.</exception>
        public void IntersectWith(IEnumerable<T> other)
        {
            if (other == null) throw new ArgumentNullException("other");
            if (Count == 0 || (object)other == (object)this) return;
            ICollection<T> collection = other as ICollection<T>;
            if (collection != null && collection.Count == 0)
            {
                Clear();
                return;
            }
            bool[] keep = MarkPresent(other);
            for (int slot = 0; slot < used; slot++)
            {
                if (hashes[slot] >= 0 && !keep[slot]) Remove(slots[slot]);
            }
        }

        /// <summary>Keeps the elements that are in this set or in <paramref name="other"/>, but not in both.</summary>
        /// <param name="other">The collection to combine with.</param>
        /// <exception cref="ArgumentNullException"><paramref name="other"/> is null.</exception>
        public void SymmetricExceptWith(IEnumerable<T> other)
        {
            if (other == null) throw new ArgumentNullException("other");
            if (Count == 0)
            {
                UnionWith(other);
                return;
            }
            if ((object)other == (object)this)
            {
                Clear();
                return;
            }
            HashSet<T> otherSet = other as HashSet<T>;
            if (otherSet != null && comparer.Equals(otherSet.comparer))
            {
                foreach (T item in otherSet)
                {
                    if (!Remove(item)) Add(item);
                }
                return;
            }
            int original = used;
            bool[] addedFromOther = new bool[original];
            bool[] shared = new bool[original];
            foreach (T item in other)
            {
                int hash = HashOf(item);
                int slot = FindSlot(item, hash);
                if (slot < 0)
                {
                    slot = Insert(item, hash);
                    if (slot < original) addedFromOther[slot] = true;
                }
                else if (slot < original && !addedFromOther[slot])
                {
                    shared[slot] = true;
                }
            }
            for (int slot = 0; slot < original; slot++)
            {
                if (shared[slot]) Remove(slots[slot]);
            }
        }

        /// <summary>Whether every element of this set is in <paramref name="other"/>.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when this set is a subset of <paramref name="other"/>.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="other"/> is null.</exception>
        public bool IsSubsetOf(IEnumerable<T> other)
        {
            if (other == null) throw new ArgumentNullException("other");
            if (Count == 0 || (object)other == (object)this) return true;
            int found;
            int missing;
            CountAgainst(other, out found, out missing);
            return found == Count;
        }

        /// <summary>Whether every element of this set is in <paramref name="other"/>, which holds at least one more.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when this set is a proper subset of <paramref name="other"/>.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="other"/> is null.</exception>
        public bool IsProperSubsetOf(IEnumerable<T> other)
        {
            if (other == null) throw new ArgumentNullException("other");
            if ((object)other == (object)this) return false;
            int found;
            int missing;
            CountAgainst(other, out found, out missing);
            return found == Count && missing > 0;
        }

        /// <summary>Whether every element of <paramref name="other"/> is in this set.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when this set is a superset of <paramref name="other"/>.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="other"/> is null.</exception>
        public bool IsSupersetOf(IEnumerable<T> other)
        {
            if (other == null) throw new ArgumentNullException("other");
            if ((object)other == (object)this) return true;
            foreach (T item in other)
            {
                if (!Contains(item)) return false;
            }
            return true;
        }

        /// <summary>Whether every element of <paramref name="other"/> is in this set, which holds at least one more.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when this set is a proper superset of <paramref name="other"/>.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="other"/> is null.</exception>
        public bool IsProperSupersetOf(IEnumerable<T> other)
        {
            if (other == null) throw new ArgumentNullException("other");
            if (Count == 0 || (object)other == (object)this) return false;
            int found;
            int missing;
            CountAgainst(other, out found, out missing);
            return missing == 0 && found < Count;
        }

        /// <summary>Whether this set and <paramref name="other"/> share at least one element.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when the two overlap.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="other"/> is null.</exception>
        public bool Overlaps(IEnumerable<T> other)
        {
            if (other == null) throw new ArgumentNullException("other");
            if (Count == 0) return false;
            if ((object)other == (object)this) return true;
            foreach (T item in other)
            {
                if (Contains(item)) return true;
            }
            return false;
        }

        /// <summary>Whether this set and <paramref name="other"/> hold the same elements, ignoring order and duplicates.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when the two are equal as sets.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="other"/> is null.</exception>
        public bool SetEquals(IEnumerable<T> other)
        {
            if (other == null) throw new ArgumentNullException("other");
            if ((object)other == (object)this) return true;
            int found;
            int missing;
            CountAgainst(other, out found, out missing);
            return found == Count && missing == 0;
        }

        /// <summary>A comparer that tests two sets for equal elements and hashes a set by its elements.</summary>
        /// <returns>The comparer.</returns>
        public static IEqualityComparer<HashSet<T>> CreateSetComparer()
        {
            return new HashSetEqualityComparer<T>();
        }

        // ---- the table -----------------------------------------------------------------------------

        private void CountAgainst(IEnumerable<T> other, out int found, out int missing)
        {
            found = 0;
            missing = 0;
            bool[] seen = used == 0 ? null : new bool[used];
            foreach (T item in other)
            {
                int slot = buckets == null ? -1 : FindSlot(item, HashOf(item));
                if (slot < 0)
                {
                    missing = missing + 1;
                }
                else if (!seen[slot])
                {
                    seen[slot] = true;
                    found = found + 1;
                }
            }
        }

        private bool[] MarkPresent(IEnumerable<T> other)
        {
            bool[] present = new bool[used];
            foreach (T item in other)
            {
                int slot = FindSlot(item, HashOf(item));
                if (slot >= 0) present[slot] = true;
            }
            return present;
        }

        private void Allocate(int capacity)
        {
            if (capacity > 0x40000000) throw new OutOfMemoryException();
            int size = 4;
            int bits = 2;
            while (size < capacity)
            {
                size = size * 2;
                bits = bits + 1;
            }
            buckets = new int[size];
            hashes = new int[size];
            next = new int[size];
            slots = new T[size];
            shift = 32 - bits;
        }

        private void Grow()
        {
            int size = hashes.Length * 2;
            if (size <= 0) throw new OutOfMemoryException();
            int[] biggerHashes = new int[size];
            int[] biggerNext = new int[size];
            T[] biggerSlots = new T[size];
            for (int slot = 0; slot < used; slot++)
            {
                biggerHashes[slot] = hashes[slot];
                biggerSlots[slot] = slots[slot];
            }
            hashes = biggerHashes;
            next = biggerNext;
            slots = biggerSlots;
            buckets = new int[size];
            shift = shift - 1;
            for (int slot = 0; slot < used; slot++)
            {
                if (hashes[slot] < 0) continue;
                int bucket = BucketOf(hashes[slot]);
                next[slot] = buckets[bucket] - 1;
                buckets[bucket] = slot + 1;
            }
        }

        private void AddNew(T item, int hash)
        {
            int slot = used;
            used = used + 1;
            int bucket = BucketOf(hash);
            hashes[slot] = hash;
            slots[slot] = item;
            next[slot] = buckets[bucket] - 1;
            buckets[bucket] = slot + 1;
        }

        private void FreeSlotAt(int slot)
        {
            hashes[slot] = FreeSlot;
            next[slot] = freeCount > 0 ? freeHead : -1;
            slots[slot] = default(T);
            freeHead = slot;
            freeCount = freeCount + 1;
        }

        private int HashOf(T item)
        {
            return comparer.GetHashCode(item) & 0x7FFFFFFF;
        }

        private int BucketOf(int hash)
        {
            return (int)(unchecked((uint)hash * 2654435769u) >> shift);
        }

        private int FindSlot(T item, int hash)
        {
            int slot = buckets[BucketOf(hash)] - 1;
            while (slot >= 0)
            {
                if (hashes[slot] == hash && comparer.Equals(slots[slot], item)) return slot;
                slot = next[slot];
            }
            return -1;
        }

        // ---- what the enumerator reads -------------------------------------------------------------

        private int NextOccupied(int expectedVersion, int from)
        {
            CheckVersion(expectedVersion);
            int slot = from;
            while (slot < used && hashes[slot] < 0) slot = slot + 1;
            return slot < used ? slot : -1;
        }

        private void CheckVersion(int expectedVersion)
        {
            if (expectedVersion != version)
            {
                throw new InvalidOperationException("Collection was modified; enumeration operation may not execute.");
            }
        }

        private int CurrentVersion
        {
            get { return version; }
        }

        private T ItemAt(int slot)
        {
            return slots[slot];
        }

        private T EmptyItem()
        {
            return default(T);
        }

        /// <summary>Enumerates the elements of a <see cref="HashSet{T}"/>.</summary>
        public struct Enumerator : IEnumerator<T>, IDisposable, IEnumerator
        {
            private HashSet<T> set;
            private int version;
            private int index;
            private bool positioned;
            private T current;

            internal Enumerator(HashSet<T> set)
            {
                this.set = set;
                version = set.CurrentVersion;
                index = 0;
                positioned = false;
                current = set.EmptyItem();
            }

            /// <summary>Moves to the next element.</summary>
            /// <returns>True when there is one.</returns>
            /// <exception cref="InvalidOperationException">The set was changed since the enumerator was made.</exception>
            public bool MoveNext()
            {
                int slot = set.NextOccupied(version, index);
                if (slot < 0)
                {
                    index = int.MaxValue;
                    positioned = false;
                    current = set.EmptyItem();
                    return false;
                }
                current = set.ItemAt(slot);
                index = slot + 1;
                positioned = true;
                return true;
            }

            /// <summary>The element at the enumerator's position.</summary>
            public T Current
            {
                get { return current; }
            }

            object IEnumerator.Current
            {
                get
                {
                    if (!positioned)
                    {
                        throw new InvalidOperationException("Enumeration has either not started or has already finished.");
                    }
                    return current;
                }
            }

            void IEnumerator.Reset()
            {
                set.CheckVersion(version);
                index = 0;
                positioned = false;
                current = set.EmptyItem();
            }

            /// <summary>Releases nothing; an enumerator holds no resource.</summary>
            public void Dispose()
            {
            }
        }
    }

    internal sealed class HashSetEqualityComparer<T> : IEqualityComparer<HashSet<T>>
    {
        public bool Equals(HashSet<T> x, HashSet<T> y)
        {
            if ((object)x == null) return (object)y == null;
            if ((object)y == null) return false;
            if (x.Count != y.Count) return false;
            EqualityComparer<T> elements = EqualityComparer<T>.Default;
            foreach (T item in y)
            {
                bool present = false;
                foreach (T mine in x)
                {
                    if (elements.Equals(mine, item))
                    {
                        present = true;
                        break;
                    }
                }
                if (!present) return false;
            }
            return true;
        }

        public int GetHashCode(HashSet<T> set)
        {
            if ((object)set == null) return 0;
            EqualityComparer<T> elements = EqualityComparer<T>.Default;
            int hash = 0;
            foreach (T item in set)
            {
                hash = unchecked(hash + elements.GetHashCode(item));
            }
            return hash;
        }
    }
}
#endif
