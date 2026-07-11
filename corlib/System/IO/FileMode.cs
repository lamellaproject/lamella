// Lamella managed corlib (from scratch). -- System.IO.FileMode / System.IO.FileAccess
#if LAMELLA_SURFACE_FILE_IO
namespace System.IO
{
    public enum FileMode
    {
        CreateNew = 1,
        Create = 2,
        Open = 3,
        OpenOrCreate = 4,
        Truncate = 5,
        Append = 6,
    }

    public enum FileAccess
    {
        Read = 1,
        Write = 2,
        ReadWrite = 3,
    }
}
#endif
