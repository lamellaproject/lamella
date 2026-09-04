// Lamella managed corlib (from scratch). -- System.Diagnostics.DebuggableAttribute
namespace System.Diagnostics
{
    /// <summary>Records the debugging and optimization settings an assembly was built with.</summary>
    [AttributeUsage(AttributeTargets.Assembly | AttributeTargets.Module, AllowMultiple = false, Inherited = true)]
    public sealed class DebuggableAttribute : Attribute
    {
#if LAMELLA_SURFACE_NETFX_2_0
        /// <summary>How a module should be debugged.</summary>
        [Flags]
        public enum DebuggingModes
        {
            /// <summary>No special settings.</summary>
            None = 0x0,
            /// <summary>The runtime tracks debugging information.</summary>
            Default = 0x1,
            /// <summary>Sequence points in the symbol store are ignored.</summary>
            IgnoreSymbolStoreSequencePoints = 0x2,
            /// <summary>Edit-and-continue is enabled.</summary>
            EnableEditAndContinue = 0x4,
            /// <summary>The optimizer is disabled.</summary>
            DisableOptimizations = 0x100
        }

        private DebuggingModes _modes;
#endif
        private bool _isJITTrackingEnabled;
        private bool _isJITOptimizerDisabled;

        /// <summary>Initializes the attribute from the tracking and optimizer settings.</summary>
        public DebuggableAttribute(bool isJITTrackingEnabled, bool isJITOptimizerDisabled)
        {
            _isJITTrackingEnabled = isJITTrackingEnabled;
            _isJITOptimizerDisabled = isJITOptimizerDisabled;
        }

        /// <summary>Whether the runtime tracks debugging information.</summary>
        public bool IsJITTrackingEnabled { get { return _isJITTrackingEnabled; } }

        /// <summary>Whether the optimizer was disabled.</summary>
        public bool IsJITOptimizerDisabled { get { return _isJITOptimizerDisabled; } }

#if LAMELLA_SURFACE_NETFX_2_0
        /// <summary>Initializes the attribute from the debugging modes.</summary>
        public DebuggableAttribute(DebuggingModes modes)
        {
            _modes = modes;
            _isJITTrackingEnabled = (modes & DebuggingModes.Default) != DebuggingModes.None;
            _isJITOptimizerDisabled = (modes & DebuggingModes.DisableOptimizations) != DebuggingModes.None;
        }

        /// <summary>The debugging modes the assembly was built with.</summary>
        public DebuggingModes DebuggingFlags { get { return _modes; } }
#endif
    }
}
