// Lamella managed corlib (from scratch). -- System.Reflection.MethodInfo
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public class MethodInfo : MethodBase
    {
        protected MethodInfo() { }

        public System.Type ReturnType
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }

        public ParameterInfo ReturnParameter
        {
            get { return new ParameterInfo(this, -1, ReturnType, null); }
        }
    }
}
#endif
