// Lamella managed corlib (from scratch). -- System.Version
namespace System
{
    public sealed class Version : IComparable
    {
        private int _major;
        private int _minor;
        private int _build;
        private int _revision;

        public Version(int major, int minor)
        {
            _major = major; _minor = minor; _build = -1; _revision = -1;
        }

        public Version(int major, int minor, int build)
        {
            _major = major; _minor = minor; _build = build; _revision = -1;
        }

        public Version(int major, int minor, int build, int revision)
        {
            _major = major; _minor = minor; _build = build; _revision = revision;
        }

        public int Major { get { return _major; } }
        public int Minor { get { return _minor; } }
        public int Build { get { return _build; } }
        public int Revision { get { return _revision; } }

        public override string ToString()
        {
            string text = _major.ToString() + "." + _minor.ToString();
            if (_build >= 0)
            {
                text = text + "." + _build.ToString();
                if (_revision >= 0) text = text + "." + _revision.ToString();
            }
            return text;
        }

        public int CompareTo(Version value)
        {
            if ((object)value == null) return 1;
            if (_major != value._major) return _major > value._major ? 1 : -1;
            if (_minor != value._minor) return _minor > value._minor ? 1 : -1;
            if (_build != value._build) return _build > value._build ? 1 : -1;
            if (_revision != value._revision) return _revision > value._revision ? 1 : -1;
            return 0;
        }

        public int CompareTo(object version)
        {
            if (version == null) return 1;
            Version other = version as Version;
            if ((object)other == null) throw new ArgumentException("Argument must be of type Version.");
            return CompareTo(other);
        }

        public bool Equals(Version obj)
        {
            if ((object)obj == null) return false;
            return _major == obj._major && _minor == obj._minor
                && _build == obj._build && _revision == obj._revision;
        }

        public override bool Equals(object obj)
        {
            return Equals(obj as Version);
        }

        public override int GetHashCode()
        {
            return (_major & 0x0F) << 28 | (_minor & 0xFF) << 20 | (_build & 0xFF) << 12 | (_revision & 0x0FFF);
        }
    }
}
