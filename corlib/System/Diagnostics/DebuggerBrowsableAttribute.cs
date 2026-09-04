// Lamella managed corlib (from scratch). -- System.Diagnostics.DebuggerBrowsableAttribute
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Diagnostics
{
    /// <summary>Determines how a debugger displays the marked member.</summary>
    [AttributeUsage(AttributeTargets.Property | AttributeTargets.Field, AllowMultiple = false, Inherited = true)]
    public sealed class DebuggerBrowsableAttribute : Attribute
    {
        private DebuggerBrowsableState _state;

        /// <summary>Initializes the attribute with the display state.</summary>
        public DebuggerBrowsableAttribute(DebuggerBrowsableState state) { _state = state; }

        /// <summary>The display state.</summary>
        public DebuggerBrowsableState State { get { return _state; } }
    }
}
#endif
