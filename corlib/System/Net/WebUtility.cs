// Lamella managed corlib (from scratch). -- System.Net.WebUtility
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Net
{
    /// <summary>Encodes text for use in web requests.</summary>
    public static class WebUtility
    {
#if LAMELLA_SURFACE_NETFX_4_5
        /// <summary>Encodes text for an HTML form or a URL query string: a space becomes '+', letters,
        /// digits and - _ . ! * ( ) are left as they are, and every other character is written as the
        /// %XX of each of its UTF-8 bytes, in uppercase hex.</summary>
        /// <param name="value">The text to encode.</param>
        /// <returns>The encoded text, the same instance when nothing needed encoding, or null when
        /// <paramref name="value"/> is null.</returns>
        public static string UrlEncode(string value)
        {
            if ((object)value == null) return null;
            return PercentEncoding.UrlEncode(value);
        }
#endif
    }
}
#endif
