// Lamella managed corlib (from scratch). -- System.IO.Directory / System.IO.DirectoryInfo
#if LAMELLA_SURFACE_FILE_IO
namespace System.IO
{
    public static class Directory
    {
        public static bool Exists(string path)
        {
            if ((object)path == null || path.Length == 0) return false;
            return NativeFs.DirExists(path) != 0;
        }

        public static DirectoryInfo CreateDirectory(string path)
        {
            if ((object)path == null) throw new ArgumentNullException("path");
            if (path.Length == 0) throw new ArgumentException("Path cannot be the empty string or all whitespace.");
            int code = NativeFs.CreateDir(path);
            if (code < 0) NativeFs.Throw(code, path);
            return new DirectoryInfo(path);
        }

        public static void Delete(string path)
        {
            Delete(path, false);
        }

        public static void Delete(string path, bool recursive)
        {
            if ((object)path == null) throw new ArgumentNullException("path");
            int code = NativeFs.DeleteDir(path, recursive);
            if (code < 0) NativeFs.Throw(code, path);
        }

        public static void Move(string sourceDirName, string destDirName)
        {
            if ((object)sourceDirName == null) throw new ArgumentNullException("sourceDirName");
            if ((object)destDirName == null) throw new ArgumentNullException("destDirName");
            if (!Exists(sourceDirName))
                throw new DirectoryNotFoundException("Could not find a part of the path '" + sourceDirName + "'.");
            int code = NativeFs.Move(sourceDirName, destDirName);
            if (code < 0) NativeFs.Throw(code, destDirName);
        }

        public static string[] GetFiles(string path)
        {
            return List(path, false);
        }

        public static string[] GetDirectories(string path)
        {
            return List(path, true);
        }

        private static string[] List(string path, bool dirsOnly)
        {
            if ((object)path == null) throw new ArgumentNullException("path");
            string[] names = NativeFs.List(path, dirsOnly);
            if ((object)names == null)
            {
                if (!Exists(path))
                    throw new DirectoryNotFoundException("Could not find a part of the path '" + path + "'.");
                throw new IOException("An I/O error occurred while accessing '" + path + "'.");
            }
            string[] joined = new string[names.Length];
            for (int i = 0; i < names.Length; i++)
                joined[i] = Path.Combine(path, names[i]);
            return joined;
        }
    }

    public class DirectoryInfo
    {
        private string _path;

        public DirectoryInfo(string path)
        {
            if ((object)path == null) throw new ArgumentNullException("path");
            _path = path;
        }

        public string FullName { get { return _path; } }

        public string Name
        {
            get
            {
                string name = Path.GetFileName(_path);
                return name.Length == 0 ? _path : name;
            }
        }

        public bool Exists { get { return Directory.Exists(_path); } }

        public override string ToString() { return _path; }
    }
}
#endif
