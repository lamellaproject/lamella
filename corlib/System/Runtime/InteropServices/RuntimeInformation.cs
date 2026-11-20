// Lamella managed corlib (from scratch). -- System.Runtime.InteropServices.RuntimeInformation
#if LAMELLA_SURFACE_NETCORE_2_0
namespace System.Runtime.InteropServices
{
    /// <summary>What the program can learn about the platform it runs on.</summary>
    public static class RuntimeInformation
    {
        /// <summary>Whether the program runs on the given platform; false for every platform .NET names.</summary>
        /// <exception cref="ArgumentNullException">The platform has no name (a default <see cref="OSPlatform"/>).</exception>
        public static bool IsOSPlatform(OSPlatform osPlatform)
        {
            string name = osPlatform.Name;
            if ((object)name == null) throw new ArgumentNullException("platform");
            return IsCurrentPlatform(name);
        }

        /// <summary>A description of the operating system: <c>Lamella</c>.</summary>
        public static string OSDescription { get { return "Lamella"; } }

        internal static bool IsCurrentPlatform(string name) { return false; }
    }
}
#endif
