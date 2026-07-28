// nanoFramework.System.IO.FileSystem (a nanoFramework compatibility assembly) -- System.IO.DriveType.
namespace System.IO
{
    /// <summary>Defines constants for drive types. The values match the nano-tier surface, which
    /// renumbers the desktop .NET enum.</summary>
    public enum DriveType
    {
        /// <summary>The medium is unknown.</summary>
        Unknown = 0,
        /// <summary>The medium has no root directory.</summary>
        NoRootDirectory = 1,
        /// <summary>Removable storage -- an SD card, a USB stick.</summary>
        Removable = 2,
        /// <summary>A fixed disk.</summary>
        Fixed = 3,
        /// <summary>A RAM-backed volume.</summary>
        Ram = 4
    }
}
