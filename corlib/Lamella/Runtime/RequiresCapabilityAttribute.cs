// Lamella managed corlib (from scratch). -- Lamella.Runtime.RequiresCapabilityAttribute
namespace Lamella.Runtime
{
    [System.AttributeUsage(System.AttributeTargets.Assembly, AllowMultiple = true)]
    public sealed class RequiresCapabilityAttribute : System.Attribute
    {
        private readonly string _capability;

        public RequiresCapabilityAttribute(string capability)
        {
            _capability = capability;
        }

        public string Capability
        {
            get { return _capability; }
        }
    }
}
