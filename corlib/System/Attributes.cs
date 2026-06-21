// Lamella managed corlib (from scratch). -- System attribute stubs
namespace System
{
    public enum AttributeTargets { All = 32767 }
    public class Attribute { public Attribute() { } }
    public sealed class AttributeUsageAttribute : Attribute
    {
        private bool _allowMultiple;
        private bool _inherited;
        public AttributeUsageAttribute(AttributeTargets validOn) { }
        public bool AllowMultiple { get { return _allowMultiple; } set { _allowMultiple = value; } }
        public bool Inherited { get { return _inherited; } set { _inherited = value; } }
    }
    public sealed class ParamArrayAttribute : Attribute { public ParamArrayAttribute() { } }
}
