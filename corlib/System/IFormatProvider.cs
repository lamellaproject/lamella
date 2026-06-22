// Lamella managed corlib (from scratch). -- System.IFormatProvider
namespace System
{
    public interface IFormatProvider
    {
        object GetFormat(Type formatType);
    }
}
