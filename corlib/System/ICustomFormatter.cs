// Lamella managed corlib (from scratch). -- System.ICustomFormatter
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    /// <summary>Formats a value under the control of a format provider, ahead of the value's own
    /// formatting in a composite format.</summary>
    public interface ICustomFormatter
    {
        /// <summary>Formats <paramref name="arg"/>.</summary>
        /// <param name="format">The format string, or null.</param>
        /// <param name="arg">The value.</param>
        /// <param name="formatProvider">The provider that supplied this formatter.</param>
        /// <returns>The text, or null to leave the value to its own formatting.</returns>
        string Format(string format, object arg, IFormatProvider formatProvider);
    }
}
#endif
