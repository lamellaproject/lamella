// Lamella managed corlib (from scratch). -- System.IO.DirectoryNotFoundException
namespace System.IO
{
    public class DirectoryNotFoundException : IOException
    {
        public DirectoryNotFoundException() : base("Attempted to access a path that is not on the disk.") { }
        public DirectoryNotFoundException(string message) : base(message) { }
        public DirectoryNotFoundException(string message, Exception innerException) : base(message, innerException) { }
    }
}
