// Lamella managed corlib (from scratch). -- System.IO.FileNotFoundException
namespace System.IO
{
    public class FileNotFoundException : IOException
    {
        private string _fileName;

        public FileNotFoundException() : base("Unable to find the specified file.") { }
        public FileNotFoundException(string message) : base(message) { }
        public FileNotFoundException(string message, Exception innerException) : base(message, innerException) { }

        public FileNotFoundException(string message, string fileName) : base(message)
        {
            _fileName = fileName;
        }

        public string FileName { get { return _fileName; } }
    }
}
