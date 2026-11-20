// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.ExtensionAttribute
namespace System.Runtime.CompilerServices
{
    /// <summary>Marks an extension method, and a class or an assembly that declares extension methods.</summary>
    [System.AttributeUsage(System.AttributeTargets.Method | System.AttributeTargets.Class | System.AttributeTargets.Assembly)]
    public sealed class ExtensionAttribute : System.Attribute
    {
        /// <summary>Initializes the attribute.</summary>
        public ExtensionAttribute()
        {
        }
    }
}
