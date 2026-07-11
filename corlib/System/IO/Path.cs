// Lamella managed corlib (from scratch). -- System.IO.Path
#if LAMELLA_SURFACE_FILE_IO
namespace System.IO
{
    public static class Path
    {
        public static readonly char DirectorySeparatorChar = '\\';
        public static readonly char AltDirectorySeparatorChar = '/';

        public static string Combine(string path1, string path2)
        {
            if ((object)path1 == null) throw new ArgumentNullException("path1");
            if ((object)path2 == null) throw new ArgumentNullException("path2");
            if (path2.Length == 0) return path1;
            if (path1.Length == 0 || IsPathRooted(path2)) return path2;
            char last = path1[path1.Length - 1];
            if (last == DirectorySeparatorChar || last == AltDirectorySeparatorChar || last == ':')
                return path1 + path2;
            return path1 + DirectorySeparatorChar + path2;
        }

        private static int NameStart(string path)
        {
            for (int i = path.Length - 1; i >= 0; i--)
            {
                char c = path[i];
                if (c == DirectorySeparatorChar || c == AltDirectorySeparatorChar || c == ':')
                    return i + 1;
            }
            return 0;
        }

        public static string GetFileName(string path)
        {
            if ((object)path == null) return null;
            return path.Substring(NameStart(path));
        }

        public static string GetDirectoryName(string path)
        {
            if ((object)path == null) return null;
            int start = NameStart(path);
            if (start == 0) return "";
            string directory = path.Substring(0, start);
            int end = directory.Length;
            while (end > 0
                && (directory[end - 1] == DirectorySeparatorChar
                    || directory[end - 1] == AltDirectorySeparatorChar))
                end--;
            if (end == 0) return null;
            if (end == 2 && directory[1] == ':') return null;
            return directory.Substring(0, end);
        }

        public static string GetExtension(string path)
        {
            if ((object)path == null) return null;
            int start = NameStart(path);
            for (int i = path.Length - 1; i >= start; i--)
            {
                if (path[i] == '.')
                    return i == path.Length - 1 ? "" : path.Substring(i);
            }
            return "";
        }

        public static string GetFileNameWithoutExtension(string path)
        {
            string name = GetFileName(path);
            if ((object)name == null) return null;
            for (int i = name.Length - 1; i >= 0; i--)
            {
                if (name[i] == '.') return name.Substring(0, i);
            }
            return name;
        }

        public static string ChangeExtension(string path, string extension)
        {
            if ((object)path == null) return null;
            string stripped = path;
            int start = NameStart(path);
            for (int i = path.Length - 1; i >= start; i--)
            {
                if (path[i] == '.')
                {
                    stripped = path.Substring(0, i);
                    break;
                }
            }
            if ((object)extension == null) return stripped;
            if (extension.Length == 0 || extension[0] != '.') return stripped + "." + extension;
            return stripped + extension;
        }

        public static bool HasExtension(string path)
        {
            if ((object)path == null) return false;
            string extension = GetExtension(path);
            return extension.Length > 0;
        }

        public static bool IsPathRooted(string path)
        {
            if ((object)path == null || path.Length == 0) return false;
            char first = path[0];
            if (first == DirectorySeparatorChar || first == AltDirectorySeparatorChar) return true;
            return path.Length >= 2 && path[1] == ':';
        }

        public static string GetTempPath()
        {
            string temp = Environment.GetEnvironmentVariable("TEMP");
            if ((object)temp == null) temp = Environment.GetEnvironmentVariable("TMP");
            if ((object)temp == null) return "\\temp\\";
            char last = temp[temp.Length - 1];
            if (last == DirectorySeparatorChar || last == AltDirectorySeparatorChar) return temp;
            return temp + DirectorySeparatorChar;
        }
    }
}
#endif
