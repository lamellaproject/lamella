// Lamella managed corlib (from scratch). -- System.IO.NativeFs (the file-system seam)
#if LAMELLA_SURFACE_FILE_IO
namespace System.IO
{
    internal sealed class NativeFs
    {
        private NativeFs() { }

        internal const int ErrNotFound = -2;
        internal const int ErrDirectoryNotFound = -3;
        internal const int ErrAccessDenied = -4;
        internal const int ErrAlreadyExists = -5;
        internal const int ErrIsDirectory = -6;
        internal const int ErrNotEmpty = -7;
        internal const int ErrInvalidPath = -8;
        internal const int ErrIo = -9;

        internal static void Throw(int code, string path)
        {
            if (code == ErrNotFound)
                throw new FileNotFoundException("Could not find file '" + path + "'.", path);
            if (code == ErrDirectoryNotFound)
                throw new DirectoryNotFoundException("Could not find a part of the path '" + path + "'.");
            if (code == ErrAccessDenied || code == ErrIsDirectory)
                throw new UnauthorizedAccessException("Access to the path '" + path + "' is denied.");
            if (code == ErrAlreadyExists)
                throw new IOException("The file '" + path + "' already exists.");
            if (code == ErrNotEmpty)
                throw new IOException("The directory is not empty. : '" + path + "'");
            if (code == ErrInvalidPath)
                throw new ArgumentException("The path is not of a legal form.");
            throw new IOException("An I/O error occurred while accessing '" + path + "'.");
        }

        [Lamella.Runtime.RuntimeProvided] internal static int Open(string path, int mode, int access) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Read(int handle, byte[] buffer, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Write(int handle, byte[] buffer, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static long Seek(int handle, long offset, int origin) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static long Length(int handle) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int SetLength(int handle, long length) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Flush(int handle) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static void Close(int handle) { }
        [Lamella.Runtime.RuntimeProvided] internal static int FileExists(string path) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int DirExists(string path) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int DeleteFile(string path) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int CreateDir(string path) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int DeleteDir(string path, bool recursive) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int Move(string from, string to) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static string[] List(string path, bool dirsOnly) { return null; }
    }
}
#endif
