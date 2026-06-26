// Lamella managed corlib (from scratch). -- System.Net.IPHostEntry
#if LAMELLA_SURFACE_NET
namespace System.Net
{
    public class IPHostEntry
    {
        private string _hostName;
        private IPAddress[] _addressList;
        private string[] _aliases;

        public string HostName
        {
            get { return _hostName; }
            set { _hostName = value; }
        }

        public IPAddress[] AddressList
        {
            get { return _addressList; }
            set { _addressList = value; }
        }

        public string[] Aliases
        {
            get { return _aliases; }
            set { _aliases = value; }
        }
    }
}
#endif
