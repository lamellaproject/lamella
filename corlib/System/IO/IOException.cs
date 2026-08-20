// Lamella managed corlib (from scratch). -- System.IO.IOException
namespace System.IO
{
    public class IOException : SystemException
    {
        public IOException() : base() { }
        public IOException(string message) : base(message) { }
        public IOException(string message, Exception innerException) : base(message, innerException) { }

        public enum IOExceptionErrorCode
        {
            TooManyOpenHandles = -385875968,
            PathAlreadyExists = -402653184,
            UnauthorizedAccess = -419430400,
            DirectoryNotEmpty = -436207616,
            PathTooLong = -452984832,
            VolumeNotFound = -469762048,
            DirectoryNotFound = -486539264,
            FileNotFound = -503316480,
            InvalidDriver = -520093696,
            Others = -536870912
        }
    }
}
