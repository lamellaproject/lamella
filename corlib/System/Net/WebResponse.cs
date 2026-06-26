// Lamella managed corlib (from scratch). -- System.Net.WebResponse
#if LAMELLA_SURFACE_NET
using System.IO;

namespace System.Net
{
    public abstract class WebResponse : IDisposable
    {
        public abstract long ContentLength { get; }
        public abstract string ContentType { get; }
        public abstract WebHeaderCollection Headers { get; }

        public abstract Stream GetResponseStream();

        public abstract void Close();

        public void Dispose() { Close(); }
    }
}
#endif
