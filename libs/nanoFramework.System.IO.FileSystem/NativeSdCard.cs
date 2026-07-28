// nanoFramework.System.IO.FileSystem (a nanoFramework compatibility assembly) -- the SDCard mount seam.

using System;
using System.IO;
namespace nanoFramework.System.IO.FileSystem
{
    internal sealed class NativeSdCard
    {
        private NativeSdCard() { }

        internal const int ErrInvalidPath = -8;
        internal const int ErrIo = -9;
        internal const int ErrUnsupported = -10;

        internal static void Throw(int code, string mountPoint)
        {
            if (code == ErrUnsupported)
                throw new NotSupportedException(
                    "Mounting an SD card at '" + mountPoint + "' is not supported on this device.");
            if (code == ErrInvalidPath)
                throw new ArgumentException("Invalid mount point '" + mountPoint + "'.");
            throw new IOException(
                "An I/O error occurred while mounting the SD card at '" + mountPoint + "'.");
        }

        [Lamella.Runtime.RuntimeProvided] internal static int MountSdOverSpiBus(string mountPoint, int spiBus, int chipSelect) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Unmount(string mountPoint) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int IsMounted(string mountPoint) { return 0; }
    }
}
