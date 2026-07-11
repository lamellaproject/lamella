// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.DecimalConstantAttribute
#if LAMELLA_SURFACE_DECIMAL
namespace System.Runtime.CompilerServices
{
    [System.AttributeUsage(System.AttributeTargets.Field | System.AttributeTargets.Parameter, Inherited = false)]
    public sealed class DecimalConstantAttribute : System.Attribute
    {
        private readonly decimal _value;

        public DecimalConstantAttribute(byte scale, byte sign, uint hi, uint mid, uint low)
        {
            _value = new decimal((int)low, (int)mid, (int)hi, sign != 0, scale);
        }

        public DecimalConstantAttribute(byte scale, byte sign, int hi, int mid, int low)
        {
            _value = new decimal(low, mid, hi, sign != 0, scale);
        }

        public decimal Value { get { return _value; } }
    }
}
#endif
