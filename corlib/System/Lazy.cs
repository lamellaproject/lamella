// Lamella managed corlib (from scratch). -- System.Lazy<T>
#if LAMELLA_SURFACE_NETFX_4_0
namespace System
{
    /// <summary>A value made the first time it is asked for, and kept.</summary>
    /// <typeparam name="T">The type of the value.</typeparam>
    public class Lazy<T>
    {
        private Func<T> factory;
        private T value;
        private bool created;
        private bool making;
        private Exception failure;
        private System.Threading.LazyThreadSafetyMode mode;
#if LAMELLA_SURFACE_THREADS
        private readonly object gate = new object();
#endif

#if LAMELLA_SURFACE_REFLECTION
        /// <summary>A lazy value made by <typeparamref name="T"/>'s parameterless constructor, thread-safe.</summary>
        public Lazy()
        {
            Initialize(null, System.Threading.LazyThreadSafetyMode.ExecutionAndPublication);
        }

        /// <summary>A lazy value made by <typeparamref name="T"/>'s parameterless constructor.</summary>
        /// <param name="isThreadSafe">True for <see cref="System.Threading.LazyThreadSafetyMode.ExecutionAndPublication"/>, false for <see cref="System.Threading.LazyThreadSafetyMode.None"/>.</param>
        public Lazy(bool isThreadSafe)
        {
            Initialize(null, isThreadSafe
                ? System.Threading.LazyThreadSafetyMode.ExecutionAndPublication
                : System.Threading.LazyThreadSafetyMode.None);
        }

        /// <summary>A lazy value made by <typeparamref name="T"/>'s parameterless constructor, with the given thread safety.</summary>
        /// <param name="mode">How threads may make the value.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="mode"/> is not a defined mode.</exception>
        public Lazy(System.Threading.LazyThreadSafetyMode mode)
        {
            Initialize(null, mode);
        }
#endif

        /// <summary>A lazy value made by <paramref name="valueFactory"/>, thread-safe.</summary>
        /// <param name="valueFactory">What makes the value.</param>
        /// <exception cref="ArgumentNullException"><paramref name="valueFactory"/> is null.</exception>
        public Lazy(Func<T> valueFactory)
        {
            if (valueFactory == null) throw new ArgumentNullException("valueFactory");
            Initialize(valueFactory, System.Threading.LazyThreadSafetyMode.ExecutionAndPublication);
        }

        /// <summary>A lazy value made by <paramref name="valueFactory"/>.</summary>
        /// <param name="valueFactory">What makes the value.</param>
        /// <param name="isThreadSafe">True for <see cref="System.Threading.LazyThreadSafetyMode.ExecutionAndPublication"/>, false for <see cref="System.Threading.LazyThreadSafetyMode.None"/>.</param>
        /// <exception cref="ArgumentNullException"><paramref name="valueFactory"/> is null.</exception>
        public Lazy(Func<T> valueFactory, bool isThreadSafe)
        {
            if (valueFactory == null) throw new ArgumentNullException("valueFactory");
            Initialize(valueFactory, isThreadSafe
                ? System.Threading.LazyThreadSafetyMode.ExecutionAndPublication
                : System.Threading.LazyThreadSafetyMode.None);
        }

        /// <summary>A lazy value made by <paramref name="valueFactory"/>, with the given thread safety.</summary>
        /// <param name="valueFactory">What makes the value.</param>
        /// <param name="mode">How threads may make the value.</param>
        /// <exception cref="ArgumentNullException"><paramref name="valueFactory"/> is null.</exception>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="mode"/> is not a defined mode.</exception>
        public Lazy(Func<T> valueFactory, System.Threading.LazyThreadSafetyMode mode)
        {
            if (valueFactory == null) throw new ArgumentNullException("valueFactory");
            Initialize(valueFactory, mode);
        }

        private void Initialize(Func<T> valueFactory, System.Threading.LazyThreadSafetyMode mode)
        {
            if (mode < System.Threading.LazyThreadSafetyMode.None
                || mode > System.Threading.LazyThreadSafetyMode.ExecutionAndPublication)
            {
                throw new ArgumentOutOfRangeException("mode");
            }
            factory = valueFactory;
            this.mode = mode;
        }

        /// <summary>Whether the value has been made.</summary>
        public bool IsValueCreated
        {
            get { return created; }
        }

        /// <summary>The value, made now if it has not been yet.</summary>
        /// <exception cref="InvalidOperationException">The value factory read this property while it was making the value.</exception>
        /// <exception cref="MissingMemberException"><typeparamref name="T"/> has no public parameterless constructor, when that is what makes the value.</exception>
        public T Value
        {
            get
            {
                if (created) return value;
                if (mode == System.Threading.LazyThreadSafetyMode.PublicationOnly) return MakePublished();
#if LAMELLA_SURFACE_THREADS
                if (mode == System.Threading.LazyThreadSafetyMode.ExecutionAndPublication)
                {
                    System.Threading.Monitor.Enter(gate);
                    try
                    {
                        return MakeOnce();
                    }
                    finally
                    {
                        System.Threading.Monitor.Exit(gate);
                    }
                }
#endif
                return MakeOnce();
            }
        }

        /// <summary>The value's own string once it has been made, and a note that it has not been otherwise.</summary>
        /// <returns>The string.</returns>
        public override string ToString()
        {
            if (!created) return "Value is not created.";
            return value.ToString();
        }

        private T MakeOnce()
        {
            if (created) return value;
            if (failure != null) throw failure;
            if (making)
            {
                throw new InvalidOperationException("ValueFactory attempted to access the Value property of this instance.");
            }
            making = true;
            T made;
            try
            {
                made = Make();
            }
            catch (Exception e)
            {
                making = false;
                if (factory != null) failure = e;
                throw;
            }
            making = false;
            value = made;
            created = true;
            factory = null;
            return made;
        }

        private T MakePublished()
        {
            T made = Make();
#if LAMELLA_SURFACE_THREADS
            System.Threading.Monitor.Enter(gate);
            try
            {
                return Publish(made);
            }
            finally
            {
                System.Threading.Monitor.Exit(gate);
            }
#else
            return Publish(made);
#endif
        }

        private T Publish(T made)
        {
            if (!created)
            {
                value = made;
                created = true;
                factory = null;
            }
            return value;
        }

        private T Make()
        {
            Func<T> maker = factory;
            if (maker != null) return maker();
#if LAMELLA_SURFACE_REFLECTION
            try
            {
                return (T)Activator.CreateInstance(typeof(T));
            }
            catch (MissingMethodException)
            {
                throw new MissingMemberException("The lazily-initialized type does not have a public, parameterless constructor.");
            }
#else
            throw new MissingMemberException("The lazily-initialized type does not have a public, parameterless constructor.");
#endif
        }
    }
}
#endif
