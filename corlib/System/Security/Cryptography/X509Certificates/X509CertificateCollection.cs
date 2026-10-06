// Lamella managed corlib (from scratch). -- System.Security.Cryptography.X509Certificates.X509CertificateCollection
#if LAMELLA_SURFACE_NET_TLS
namespace System.Security.Cryptography.X509Certificates
{

    /// <summary>A collection of <see cref="X509Certificate"/> objects: the client certificates a TLS client may present.</summary>
    public class X509CertificateCollection : System.Collections.CollectionBase
    {
        /// <summary>Initializes an empty collection.</summary>
        public X509CertificateCollection() { }

        /// <summary>Initializes a collection holding the certificates of <paramref name="value"/>.</summary>
        /// <exception cref="ArgumentNullException"><paramref name="value"/> is <see langword="null"/>, or holds a <see langword="null"/>.</exception>
        public X509CertificateCollection(X509Certificate[] value)
        {
            AddRange(value);
        }

        /// <summary>Initializes a collection holding the certificates of <paramref name="value"/>.</summary>
        /// <exception cref="ArgumentNullException"><paramref name="value"/> is <see langword="null"/>.</exception>
        public X509CertificateCollection(X509CertificateCollection value)
        {
            AddRange(value);
        }

        /// <summary>The certificate at <paramref name="index"/>.</summary>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="index"/> is less than zero, or not less than <see cref="System.Collections.CollectionBase.Count"/>.</exception>
        public X509Certificate this[int index]
        {
            get { return (X509Certificate)GetItem(index); }
            set { SetItem(index, value); }
        }

        /// <summary>Adds <paramref name="value"/> at the end, and returns its index.</summary>
        public int Add(X509Certificate value)
        {
            return AddItem(value);
        }

        /// <summary>Adds each certificate of <paramref name="value"/> at the end.</summary>
        public void AddRange(X509Certificate[] value)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            for (int i = 0; i < value.Length; i++)
            {
                Add(value[i]);
            }
        }

        /// <summary>Adds each certificate of <paramref name="value"/> at the end.</summary>
        public void AddRange(X509CertificateCollection value)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            int count = value.Count;
            for (int i = 0; i < count; i++)
            {
                Add(value[i]);
            }
        }

        /// <summary>Whether the collection holds <paramref name="value"/>.</summary>
        public bool Contains(X509Certificate value)
        {
            return InnerList.Contains(value);
        }

        /// <summary>Copies the certificates into <paramref name="array"/>, starting at <paramref name="index"/>.</summary>
        public void CopyTo(X509Certificate[] array, int index)
        {
            InnerList.CopyTo(array, index);
        }

        /// <summary>The index of <paramref name="value"/>, or -1 when the collection does not hold it.</summary>
        public int IndexOf(X509Certificate value)
        {
            return InnerList.IndexOf(value);
        }

        /// <summary>Inserts <paramref name="value"/> at <paramref name="index"/>.</summary>
        public void Insert(int index, X509Certificate value)
        {
            InsertItem(index, value);
        }

        /// <summary>Removes <paramref name="value"/>.</summary>
        /// <exception cref="ArgumentException">The collection does not hold <paramref name="value"/>.</exception>
        public void Remove(X509Certificate value)
        {
            RemoveItem(value);
        }

        /// <summary>An enumerator over the certificates.</summary>
        public new X509CertificateEnumerator GetEnumerator()
        {
            return new X509CertificateEnumerator(this);
        }

        /// <summary>A hash code combined from the certificates' own.</summary>
        public override int GetHashCode()
        {
            int hash = 0;
            for (int i = 0; i < Count; i++)
            {
                hash += this[i].GetHashCode();
            }
            return hash;
        }

        /// <summary>Enumerates the certificates of an <see cref="X509CertificateCollection"/>.</summary>
        public class X509CertificateEnumerator : System.Collections.IEnumerator
        {
            private System.Collections.IEnumerator _inner;

            /// <summary>Initializes an enumerator over <paramref name="mappings"/>.</summary>
            public X509CertificateEnumerator(X509CertificateCollection mappings)
            {
                _inner = mappings.InnerList.GetEnumerator();
            }

            /// <summary>The certificate at the enumerator's position.</summary>
            public X509Certificate Current
            {
                get { return (X509Certificate)_inner.Current; }
            }

            object System.Collections.IEnumerator.Current
            {
                get { return _inner.Current; }
            }

            /// <summary>Moves to the next certificate, and says whether there was one.</summary>
            public bool MoveNext()
            {
                return _inner.MoveNext();
            }

            /// <summary>Moves back to before the first certificate.</summary>
            public void Reset()
            {
                _inner.Reset();
            }
        }
    }
}
#endif
