// nanoFramework.System.IO.FileSystem (a nanoFramework compatibility assembly) -- System.IO.DriveInfo.
namespace System.IO
{
    /// <summary>Information about a drive, in <b>nanoFramework's</b> shape -- NOT .NET's
    /// <c>System.IO.DriveInfo</c>, which this type shares a name with and does not match.</summary>
    /// <remarks>
    /// <para>The assembly carries a <c>nanoFramework.</c> prefix to say so, but the namespace is
    /// <c>System.IO</c> -- as it is in nanoFramework -- because that is what lets nanoFramework
    /// source compile here unchanged. So the namespace alone will not warn you, and the divergences
    /// run in both directions:</para>
    /// <para>Present here and NOT in .NET: <see cref="DriveType"/>, <see cref="Name"/> and
    /// <see cref="TotalSize"/> are settable, where .NET's are read-only; and
    /// <see cref="Format(string, uint)"/>, <see cref="GetFileSystems"/> and
    /// <see cref="MountRemovableVolumes"/> have no .NET counterpart at all.</para>
    /// <para>Present in .NET and NOT here: <c>DriveFormat</c>, <c>IsReady</c>,
    /// <c>RootDirectory</c>, <c>TotalFreeSpace</c>, <c>AvailableFreeSpace</c> and
    /// <c>VolumeLabel</c>.</para>
    /// <para>So code written against .NET meets different semantics here, and code written here
    /// against <c>Format</c> will not compile against .NET.</para>
    /// </remarks>
    public sealed class DriveInfo
    {
        private DriveType _driveType;
        private string _name;
        private long _totalSize;

        /// <summary>The drive type (removable, fixed, RAM, ...).</summary>
        public DriveType DriveType { get { return _driveType; } set { _driveType = value; } }

        /// <summary>The drive name, such as "D:".</summary>
        public string Name { get { return _name; } set { _name = value; } }

        /// <summary>The total size of the drive, in bytes.</summary>
        public long TotalSize { get { return _totalSize; } set { _totalSize = value; } }

        /// <summary>Creates a <see cref="DriveInfo"/> for the drive named <paramref name="driveName"/>.</summary>
        public DriveInfo(string driveName)
        {
            _name = driveName;
            Refresh();
        }

        /// <summary>Re-reads <see cref="DriveType"/> and <see cref="TotalSize"/> from the runtime.</summary>
        public void Refresh()
        {
            _driveType = (DriveType)NativeDrive.Kind(_name);
            _totalSize = NativeDrive.TotalSize(_name);
        }

        /// <summary>Formats the drive with the given file system. This operation is DESTRUCTIVE.</summary>
        public void Format(string fileSystem, uint parameter)
        {
            int code = NativeDrive.Format(_name, fileSystem, parameter);
            if (code < 0)
                NativeDrive.Throw(code, _name);
        }

        /// <summary>Retrieves every mounted drive.</summary>
        public static DriveInfo[] GetDrives()
        {
            string[] names = NativeDrive.Names();
            if (names == null)
                return new DriveInfo[0];
            DriveInfo[] drives = new DriveInfo[names.Length];
            for (int i = 0; i < names.Length; i++)
                drives[i] = new DriveInfo(names[i]);
            return drives;
        }

        /// <summary>The file-system format names this device can create (for example "FAT").</summary>
        public static string[] GetFileSystems()
        {
            string[] names = NativeDrive.FileSystems();
            if (names == null)
                return new string[0];
            return names;
        }

        /// <summary>Mounts the removable volumes the board declares.</summary>
        public static void MountRemovableVolumes()
        {
            NativeDrive.MountRemovableVolumes();
        }
    }
}
