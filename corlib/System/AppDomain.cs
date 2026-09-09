// Lamella managed corlib (from scratch). -- System.AppDomain
namespace System
{
    public sealed class AppDomain
    {
        private static AppDomain _current = new AppDomain();

        private AppDomain() { }

        public static AppDomain CurrentDomain { get { return _current; } }

        public string FriendlyName
        {
            get { return DomainFriendlyName(); }
        }

#if LAMELLA_SURFACE_REFLECTION
        public System.Reflection.Assembly[] GetAssemblies() { return DomainAssemblies(); }

        [Lamella.Runtime.RuntimeProvided] private static System.Reflection.Assembly[] DomainAssemblies() { return null; }
#endif

        [Lamella.Runtime.RuntimeProvided] private static string DomainFriendlyName() { return null; }
    }
}
