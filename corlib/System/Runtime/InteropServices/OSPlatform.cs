// Lamella managed corlib (from scratch). -- System.Runtime.InteropServices.OSPlatform
#if LAMELLA_SURFACE_NETCORE_2_0
namespace System.Runtime.InteropServices
{
    /// <summary>A platform's name, as <see cref="RuntimeInformation.IsOSPlatform"/> takes it.</summary>
    public readonly struct OSPlatform : IEquatable<OSPlatform>
    {
        private readonly string _osPlatform;

        private OSPlatform(string osPlatform) { _osPlatform = osPlatform; }

        internal string Name { get { return _osPlatform; } }

        private static readonly OSPlatform s_linux = new OSPlatform("LINUX");
        private static readonly OSPlatform s_osx = new OSPlatform("OSX");
        private static readonly OSPlatform s_windows = new OSPlatform("WINDOWS");
#if LAMELLA_SURFACE_NETCORE_3_0
        private static readonly OSPlatform s_freeBsd = new OSPlatform("FREEBSD");

        /// <summary>FreeBSD.</summary>
        public static OSPlatform FreeBSD { get { return s_freeBsd; } }
#endif

        /// <summary>Linux.</summary>
        public static OSPlatform Linux { get { return s_linux; } }

        /// <summary>macOS.</summary>
        public static OSPlatform OSX { get { return s_osx; } }

        /// <summary>Windows.</summary>
        public static OSPlatform Windows { get { return s_windows; } }

        /// <summary>A platform by name, kept as written.</summary>
        /// <exception cref="ArgumentNullException"><paramref name="osPlatform"/> is null.</exception>
        /// <exception cref="ArgumentException"><paramref name="osPlatform"/> is empty.</exception>
        public static OSPlatform Create(string osPlatform)
        {
            if ((object)osPlatform == null) throw new ArgumentNullException("osPlatform");
            if (osPlatform.Length == 0) throw new ArgumentException("The value cannot be an empty string.", "osPlatform");
            return new OSPlatform(osPlatform);
        }

        /// <summary>Whether two platforms have the same name, ignoring case.</summary>
        public bool Equals(OSPlatform other)
        {
            return string.Equals(_osPlatform, other._osPlatform, StringComparison.OrdinalIgnoreCase);
        }

        /// <summary>Whether <paramref name="obj"/> is an <see cref="OSPlatform"/> with the same name, ignoring case.</summary>
        public override bool Equals(object obj)
        {
            return obj is OSPlatform && Equals((OSPlatform)obj);
        }

        /// <summary>A hash of the name that ignores case, as equality does; 0 for a default platform.</summary>
        public override int GetHashCode()
        {
            return (object)_osPlatform == null ? 0 : _osPlatform.ToUpperInvariant().GetHashCode();
        }

        /// <summary>The platform's name, or an empty string for a default platform.</summary>
        public override string ToString()
        {
            return (object)_osPlatform == null ? string.Empty : _osPlatform;
        }

        /// <summary>Whether two platforms have the same name, ignoring case.</summary>
        public static bool operator ==(OSPlatform left, OSPlatform right) { return left.Equals(right); }

        /// <summary>Whether two platforms have different names, ignoring case.</summary>
        public static bool operator !=(OSPlatform left, OSPlatform right) { return !left.Equals(right); }
    }
}
#endif
