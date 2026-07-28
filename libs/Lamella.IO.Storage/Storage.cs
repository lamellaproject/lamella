// Lamella.IO.Storage -- the code-driven mount API.
namespace Lamella.IO
{
    /// <summary>Mounts and unmounts storage volumes from code.</summary>
    public sealed class Storage
    {
        private Storage() { }

        /// <summary>Mounts the FAT volume on the SD card wired to <paramref name="spi"/> at
        /// <paramref name="mountPoint"/>, using <paramref name="chipSelect"/> as the card's
        /// chip-select pin (-1 for a bus that frames chip-select in hardware). The volume then
        /// appears as a removable drive, and its files are reachable through
        /// <c>System.IO</c> under that prefix.</summary>
        /// <remarks>The mount TAKES OVER the bus for as long as it lasts, and drives it natively:
        /// the card is clocked by the device's own SD driver, not through
        /// <paramref name="spi"/>. Using <paramref name="spi"/> again before
        /// <see cref="Unmount"/> interleaves traffic with the card and corrupts it. Unmounting
        /// releases the bus and disposes the device.</remarks>
        /// <exception cref="System.ArgumentNullException"><paramref name="spi"/> or
        /// <paramref name="mountPoint"/> is null.</exception>
        /// <exception cref="System.NotSupportedException">This device cannot mount SD over SPI, or
        /// nothing native owns the bus <paramref name="spi"/> drives -- including a
        /// <paramref name="spi"/> built over a bit-banged or host-backed driver, which names no
        /// peripheral instance at all.</exception>
        /// <exception cref="System.ArgumentException"><paramref name="mountPoint"/> is not a usable
        /// mount point.</exception>
        /// <exception cref="System.IO.IOException">The card could not be brought up or carries no
        /// FAT volume.</exception>
        public static void MountSdOverSpi(System.Device.Spi.SpiDevice spi, int chipSelect, string mountPoint)
        {
            if ((object)spi == null) throw new System.ArgumentNullException("spi");
            if ((object)mountPoint == null) throw new System.ArgumentNullException("mountPoint");
            uint bus = Lamella.Hardware.SpiNative.IdentityOf(spi);
            if (bus == 0)
                throw new System.NotSupportedException(
                    "The SPI device names no peripheral instance a native storage driver can take over.");
            int identity = unchecked((int)bus);
            int code = NativeStorage.MountSdOverSpi(mountPoint, identity, chipSelect);
            if (code != 0) NativeStorage.Throw(code, mountPoint);
        }

        /// <summary>Removes the mount at <paramref name="mountPoint"/>, releasing its medium.
        /// Returns false when nothing was mounted there.</summary>
        /// <remarks>Files still open on the volume are NOT closed by this call; reading or writing
        /// one afterwards fails with an <see cref="System.IO.IOException"/> rather than reaching a
        /// medium that is no longer there.</remarks>
        /// <exception cref="System.ArgumentNullException"><paramref name="mountPoint"/> is
        /// null.</exception>
        public static bool Unmount(string mountPoint)
        {
            if ((object)mountPoint == null) throw new System.ArgumentNullException("mountPoint");
            return NativeStorage.Unmount(mountPoint) != 0;
        }

        /// <summary>Whether a volume is mounted at exactly <paramref name="mountPoint"/>.</summary>
        /// <exception cref="System.ArgumentNullException"><paramref name="mountPoint"/> is
        /// null.</exception>
        public static bool IsMounted(string mountPoint)
        {
            if ((object)mountPoint == null) throw new System.ArgumentNullException("mountPoint");
            return NativeStorage.IsMounted(mountPoint) != 0;
        }
    }
}
