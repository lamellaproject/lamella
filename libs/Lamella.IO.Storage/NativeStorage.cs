// Lamella.IO.Storage -- Lamella.IO.NativeStorage (the mount-table seam).
namespace Lamella.IO
{
    internal sealed class NativeStorage
    {
        private NativeStorage() { }

        internal const int ErrInvalidPath = -8;
        internal const int ErrIo = -9;
        internal const int ErrUnsupported = -10;

        internal static void Throw(int code, string mountPoint)
        {
            if (code == ErrUnsupported)
                throw new System.NotSupportedException(
                    "Mounting storage at '" + mountPoint + "' is not supported on this device.");
            if (code == ErrInvalidPath)
                throw new System.ArgumentException("Invalid mount point '" + mountPoint + "'.");
            throw new System.IO.IOException(
                "An I/O error occurred while mounting storage at '" + mountPoint + "'.");
        }

        [Lamella.Runtime.RuntimeProvided] internal static int MountSdOverSpi(string mountPoint, int busIdentity, int chipSelect) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Unmount(string mountPoint) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int IsMounted(string mountPoint) { return 0; }
    }
}
