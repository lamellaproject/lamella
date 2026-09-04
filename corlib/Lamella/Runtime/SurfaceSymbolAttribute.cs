// Lamella managed corlib (from scratch). -- Lamella.Runtime.SurfaceSymbolAttribute
namespace Lamella.Runtime
{
    [System.AttributeUsage(System.AttributeTargets.Assembly, AllowMultiple = true)]
    public sealed class SurfaceSymbolAttribute : System.Attribute
    {
        public SurfaceSymbolAttribute(string symbol)
        {
        }
    }
}
