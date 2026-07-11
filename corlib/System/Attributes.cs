// Lamella managed corlib (from scratch). -- System attribute stubs
namespace System
{
    [Flags]
    public enum AttributeTargets
    {
        Assembly = 0x0001,
        Module = 0x0002,
        Class = 0x0004,
        Struct = 0x0008,
        Enum = 0x0010,
        Constructor = 0x0020,
        Method = 0x0040,
        Property = 0x0080,
        Field = 0x0100,
        Event = 0x0200,
        Interface = 0x0400,
        Parameter = 0x0800,
        Delegate = 0x1000,
        ReturnValue = 0x2000,
        GenericParameter = 0x4000,
        All = 32767
    }
    public class Attribute { public Attribute() { } }
    [AttributeUsage(AttributeTargets.Enum, Inherited = false)]
    public sealed class FlagsAttribute : Attribute { public FlagsAttribute() { } }
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
