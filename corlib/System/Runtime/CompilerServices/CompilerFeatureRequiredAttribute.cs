// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.CompilerFeatureRequiredAttribute
namespace System.Runtime.CompilerServices
{
    [System.AttributeUsage(System.AttributeTargets.All, AllowMultiple = true, Inherited = false)]
    public sealed class CompilerFeatureRequiredAttribute : System.Attribute
    {
        private readonly string _featureName;

        public CompilerFeatureRequiredAttribute(string featureName)
        {
            _featureName = featureName;
        }

        public string FeatureName { get { return _featureName; } }

        public const string RefStructs = "RefStructs";
        public const string RequiredMembers = "RequiredMembers";
    }
}
