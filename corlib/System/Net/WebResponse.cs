// Lamella managed corlib (from scratch). -- System.Net.WebResponse
#if LAMELLA_SURFACE_NET
using System.IO;

namespace System.Net
{
    public abstract class WebResponse : IDisposable
    {
        private const string MethodNotImplemented = "This method is not implemented by this class.";
        private const string PropertyNotImplemented = "This property is not implemented by this class.";

        public virtual long ContentLength { get { throw new NotImplementedException(PropertyNotImplemented); } }
        public virtual string ContentType { get { throw new NotImplementedException(PropertyNotImplemented); } }
        public virtual WebHeaderCollection Headers { get { throw new NotImplementedException(PropertyNotImplemented); } }

        public virtual Stream GetResponseStream() { throw new NotImplementedException(MethodNotImplemented); }

        public virtual void Close() { }

        public void Dispose() { Close(); }
    }
}
#endif
