// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.RuntimeHelpers
namespace System.Runtime.CompilerServices
{
    /// <summary>Reserved for the compiler: services the runtime provides to generated code.</summary>
    public sealed class RuntimeHelpers
    {
        private RuntimeHelpers()
        {
        }

        /// <summary>Ensures the remaining stack is large enough for the average .NET function.</summary>
        /// <remarks>
        /// IMPORTANT: this runtime bounds the call stack by FRAME COUNT and raises a reported
        /// overflow at that bound, so this method always returns and never throws. Code that must
        /// react to a deep stack should handle the overflow rather than pre-check for it.
        /// </remarks>
        public static void EnsureSufficientExecutionStack()
        {
        }
    }
}
