// Lamella managed corlib (from scratch). -- System.Diagnostics.ConditionalAttribute
namespace System.Diagnostics
{
    /// <summary>Indicates that calls to the marked method are omitted unless the named
    /// conditional compilation symbol is defined at the call site.</summary>
    [AttributeUsage(AttributeTargets.Class | AttributeTargets.Method, AllowMultiple = true, Inherited = true)]
    public sealed class ConditionalAttribute : Attribute
    {
        private string _conditionString;

        /// <summary>Initializes the attribute with the conditional compilation symbol.</summary>
        public ConditionalAttribute(string conditionString) { _conditionString = conditionString; }

        /// <summary>The conditional compilation symbol.</summary>
        public string ConditionString { get { return _conditionString; } }
    }
}
