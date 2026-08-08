// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.RequiredMemberAttribute
namespace System.Runtime.CompilerServices
{
    [System.AttributeUsage(System.AttributeTargets.Class | System.AttributeTargets.Struct | System.AttributeTargets.Field | System.AttributeTargets.Property, AllowMultiple = false, Inherited = false)]
    public sealed class RequiredMemberAttribute : System.Attribute
    {
        public RequiredMemberAttribute() { }
    }
}
