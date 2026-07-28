// nanoFramework.System.IO.FileSystem (a nanoFramework compatibility assembly) -- the DriveInfo seam.
namespace System.IO
{
    internal sealed class NativeDrive
    {
        private NativeDrive() { }

        internal const int ErrInvalidPath = -8;
        internal const int ErrIo = -9;
        internal const int ErrUnsupported = -10;

        internal static void Throw(int code, string name)
        {
            if (code == ErrUnsupported)
                throw new NotSupportedException("Formatting drive '" + name + "' is not supported.");
            if (code == ErrInvalidPath)
                throw new ArgumentException("Unknown or unsupported file system.");
            throw new IOException("An I/O error occurred while formatting drive '" + name + "'.");
        }

        [Lamella.Runtime.RuntimeProvided] internal static string[] Names() { return null; }
        [Lamella.Runtime.RuntimeProvided] internal static int Kind(string name) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static long TotalSize(string name) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Format(string name, string fileSystem, uint parameter) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static string[] FileSystems() { return null; }
        [Lamella.Runtime.RuntimeProvided] internal static void MountRemovableVolumes() { }
    }
}
