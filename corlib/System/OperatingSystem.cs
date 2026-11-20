// Lamella managed corlib (from scratch). -- System.OperatingSystem
namespace System
{
    /// <summary>An operating system's family and version, as <see cref="Environment"/> reports them.</summary>
    public sealed class OperatingSystem : ICloneable
    {
        private readonly PlatformID _platform;
        private readonly Version _version;
        private string _versionString;

#if LAMELLA_SURFACE_NET_5_0
        private const PlatformID HighestPlatform = PlatformID.Other;
#elif LAMELLA_SURFACE_NETFX_2_0
        private const PlatformID HighestPlatform = PlatformID.MacOSX;
#elif LAMELLA_SURFACE_NETFX_1_1
        private const PlatformID HighestPlatform = PlatformID.WinCE;
#else
        private const PlatformID HighestPlatform = PlatformID.Win32NT;
#endif

        /// <summary>Describes an operating system by its family and version.</summary>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="platform"/> is not a member of <see cref="PlatformID"/>.</exception>
        /// <exception cref="ArgumentNullException"><paramref name="version"/> is null.</exception>
        public OperatingSystem(PlatformID platform, Version version)
        {
            if (platform < PlatformID.Win32S || platform > HighestPlatform)
            {
                throw new ArgumentOutOfRangeException("platform", platform, "Illegal enum value: " + ((int)platform).ToString() + ".");
            }
            if ((object)version == null) throw new ArgumentNullException("version");
            _platform = platform;
            _version = version;
        }

        /// <summary>The operating system's family.</summary>
        public PlatformID Platform { get { return _platform; } }

        /// <summary>The operating system's version: the same object the constructor was given.</summary>
        public Version Version { get { return _version; } }

#if LAMELLA_SURFACE_NETFX_2_0
        /// <summary>The service pack's name; empty, since only the runtime can name one.</summary>
        public string ServicePack { get { return string.Empty; } }

        /// <summary>The family's name and the version, such as <c>Unix 6.6</c> or <c>Other 1.0.0.0</c>.</summary>
        public string VersionString { get { return Text(); } }
#endif

        /// <summary>A new <see cref="OperatingSystem"/> with the same family and version.</summary>
        public object Clone() { return new OperatingSystem(_platform, _version); }

        /// <summary>The family's name and the version, as <c>VersionString</c> gives them.</summary>
        public override string ToString() { return Text(); }

        private string Text()
        {
            if ((object)_versionString == null)
            {
                string family;
                switch (_platform)
                {
                    case PlatformID.Win32S: family = "Microsoft Win32S "; break;
                    case PlatformID.Win32Windows:
                        family = _version.Major > 4 || (_version.Major == 4 && _version.Minor > 0) ? "Microsoft Windows 98 " : "Microsoft Windows 95 ";
                        break;
                    case PlatformID.Win32NT: family = "Microsoft Windows NT "; break;
#if LAMELLA_SURFACE_NETFX_1_1
                    case PlatformID.WinCE: family = "Microsoft Windows CE "; break;
#endif
#if LAMELLA_SURFACE_NETFX_2_0
                    case PlatformID.Unix: family = "Unix "; break;
                    case PlatformID.Xbox: family = "Xbox "; break;
                    case PlatformID.MacOSX: family = "Mac OS X "; break;
#endif
#if LAMELLA_SURFACE_NET_5_0
                    case PlatformID.Other: family = "Other "; break;
#endif
                    default: family = string.Empty; break;
                }
                _versionString = family + _version.ToString();
            }
            return _versionString;
        }

#if LAMELLA_SURFACE_NET_5_0
        /// <summary>Whether the program runs on the named platform; false for every platform .NET names.</summary>
        /// <exception cref="ArgumentNullException"><paramref name="platform"/> is null.</exception>
        public static bool IsOSPlatform(string platform)
        {
            if ((object)platform == null) throw new ArgumentNullException("platform");
            return System.Runtime.InteropServices.RuntimeInformation.IsCurrentPlatform(platform);
        }

        /// <summary>Whether the program runs on the named platform at the given version or later.</summary>
        /// <exception cref="ArgumentNullException"><paramref name="platform"/> is null.</exception>
        public static bool IsOSPlatformVersionAtLeast(string platform, int major, int minor = 0, int build = 0, int revision = 0)
        {
            return IsOSPlatform(platform) && IsVersionAtLeast(major, minor, build, revision);
        }

        private static bool IsVersionAtLeast(int major, int minor, int build, int revision)
        {
            Version current = Environment.OSVersion.Version;
            if (current.Major != major) return current.Major > major;
            if (current.Minor != minor) return current.Minor > minor;
            int currentBuild = current.Build < 0 ? 0 : current.Build;
            if (build < 0) build = 0;
            if (currentBuild != build) return currentBuild > build;
            int currentRevision = current.Revision < 0 ? 0 : current.Revision;
            if (revision < 0) revision = 0;
            return currentRevision >= revision;
        }

        /// <summary>Whether the program runs in a browser: false.</summary>
        public static bool IsBrowser() { return false; }
        /// <summary>Whether the program runs on Linux: false.</summary>
        public static bool IsLinux() { return false; }
        /// <summary>Whether the program runs on FreeBSD: false.</summary>
        public static bool IsFreeBSD() { return false; }
        /// <summary>Whether the program runs on FreeBSD at the given version or later: false.</summary>
        public static bool IsFreeBSDVersionAtLeast(int major, int minor = 0, int build = 0, int revision = 0) { return false; }
        /// <summary>Whether the program runs on Android: false.</summary>
        public static bool IsAndroid() { return false; }
        /// <summary>Whether the program runs on Android at the given version or later: false.</summary>
        public static bool IsAndroidVersionAtLeast(int major, int minor = 0, int build = 0, int revision = 0) { return false; }
        /// <summary>Whether the program runs on iOS: false.</summary>
        public static bool IsIOS() { return false; }
        /// <summary>Whether the program runs on iOS at the given version or later: false.</summary>
        public static bool IsIOSVersionAtLeast(int major, int minor = 0, int build = 0) { return false; }
        /// <summary>Whether the program runs on macOS: false.</summary>
        public static bool IsMacOS() { return false; }
        /// <summary>Whether the program runs on macOS at the given version or later: false.</summary>
        public static bool IsMacOSVersionAtLeast(int major, int minor = 0, int build = 0) { return false; }
        /// <summary>Whether the program runs on tvOS: false.</summary>
        public static bool IsTvOS() { return false; }
        /// <summary>Whether the program runs on tvOS at the given version or later: false.</summary>
        public static bool IsTvOSVersionAtLeast(int major, int minor = 0, int build = 0) { return false; }
        /// <summary>Whether the program runs on watchOS: false.</summary>
        public static bool IsWatchOS() { return false; }
        /// <summary>Whether the program runs on watchOS at the given version or later: false.</summary>
        public static bool IsWatchOSVersionAtLeast(int major, int minor = 0, int build = 0) { return false; }
        /// <summary>Whether the program runs on Windows: false.</summary>
        public static bool IsWindows() { return false; }
        /// <summary>Whether the program runs on Windows at the given version or later: false.</summary>
        public static bool IsWindowsVersionAtLeast(int major, int minor = 0, int build = 0, int revision = 0) { return false; }
#endif
    }
}
