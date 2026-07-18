// Lamella managed corlib (from scratch). -- System.IO.File
#if LAMELLA_SURFACE_FILE_IO
namespace System.IO
{
    public sealed class File
    {
        private File() { }

        public static bool Exists(string path)
        {
            if ((object)path == null || path.Length == 0) return false;
            return NativeFs.FileExists(path) != 0;
        }

        public static void Delete(string path)
        {
            if ((object)path == null) throw new ArgumentNullException("path");
            int code = NativeFs.DeleteFile(path);
            if (code < 0) NativeFs.Throw(code, path);
        }

        public static void Move(string sourceFileName, string destFileName)
        {
            if ((object)sourceFileName == null) throw new ArgumentNullException("sourceFileName");
            if ((object)destFileName == null) throw new ArgumentNullException("destFileName");
            if (!Exists(sourceFileName))
                throw new FileNotFoundException("Could not find file '" + sourceFileName + "'.", sourceFileName);
            int code = NativeFs.Move(sourceFileName, destFileName);
            if (code < 0) NativeFs.Throw(code, destFileName);
        }

        public static void Copy(string sourceFileName, string destFileName)
        {
            FileStream source = new FileStream(sourceFileName, FileMode.Open, FileAccess.Read);
            try
            {
                FileStream destination = new FileStream(destFileName, FileMode.CreateNew, FileAccess.Write);
                try
                {
                    byte[] shuttle = new byte[4096];
                    int read;
                    while ((read = source.Read(shuttle, 0, shuttle.Length)) > 0)
                        destination.Write(shuttle, 0, read);
                }
                finally
                {
                    destination.Close();
                }
            }
            finally
            {
                source.Close();
            }
        }

        public static FileStream Open(string path, FileMode mode)
        {
            return new FileStream(path, mode);
        }

        public static FileStream Open(string path, FileMode mode, FileAccess access)
        {
            return new FileStream(path, mode, access);
        }

        public static FileStream Create(string path)
        {
            return new FileStream(path, FileMode.Create, FileAccess.ReadWrite);
        }

        public static FileStream OpenRead(string path)
        {
            return new FileStream(path, FileMode.Open, FileAccess.Read);
        }

        public static FileStream OpenWrite(string path)
        {
            return new FileStream(path, FileMode.OpenOrCreate, FileAccess.Write);
        }

        public static StreamReader OpenText(string path)
        {
            return new StreamReader(new FileStream(path, FileMode.Open, FileAccess.Read));
        }

        public static StreamWriter CreateText(string path)
        {
            return new StreamWriter(new FileStream(path, FileMode.Create, FileAccess.Write));
        }

        public static StreamWriter AppendText(string path)
        {
            return new StreamWriter(new FileStream(path, FileMode.Append, FileAccess.Write));
        }

        public static string ReadAllText(string path)
        {
            StreamReader reader = OpenText(path);
            try
            {
                return reader.ReadToEnd();
            }
            finally
            {
                reader.Close();
            }
        }

        public static void WriteAllText(string path, string contents)
        {
            if ((object)contents == null) contents = "";
            StreamWriter writer = CreateText(path);
            try
            {
                writer.Write(contents);
            }
            finally
            {
                writer.Close();
            }
        }

        public static void AppendAllText(string path, string contents)
        {
            if ((object)contents == null) contents = "";
            StreamWriter writer = AppendText(path);
            try
            {
                writer.Write(contents);
            }
            finally
            {
                writer.Close();
            }
        }

        public static byte[] ReadAllBytes(string path)
        {
            FileStream stream = OpenRead(path);
            try
            {
                long length = stream.Length;
                byte[] bytes = new byte[(int)length];
                int total = 0;
                while (total < bytes.Length)
                {
                    int read = stream.Read(bytes, total, bytes.Length - total);
                    if (read == 0) throw new EndOfStreamException("Unable to read beyond the end of the stream.");
                    total += read;
                }
                return bytes;
            }
            finally
            {
                stream.Close();
            }
        }

        public static void WriteAllBytes(string path, byte[] bytes)
        {
            if ((object)bytes == null) throw new ArgumentNullException("bytes");
            FileStream stream = new FileStream(path, FileMode.Create, FileAccess.Write);
            try
            {
                stream.Write(bytes, 0, bytes.Length);
            }
            finally
            {
                stream.Close();
            }
        }
    }
}
#endif
