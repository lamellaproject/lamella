// Lamella managed corlib (from scratch). -- System.Reflection.ParameterInfo
namespace System.Reflection
{
    public class ParameterInfo
    {
        private MethodBase _member;
        private int _position;
        private Type _parameterType;
        private string _name;

        internal ParameterInfo(MethodBase member, int position, Type parameterType, string name)
        {
            _member = member;
            _position = position;
            _parameterType = parameterType;
            _name = name;
        }

        public int Position { get { return _position; } }
        public Type ParameterType { get { return _parameterType; } }
        public string Name { get { return _name; } }
        public MethodBase Member { get { return _member; } }

        public object[] GetCustomAttributes(bool inherit)
        {
            if ((object)_member == null) return new object[0];
            object[] all = _member.GetParameterCustomAttributes(_position, inherit);
            if ((object)all == null) return new object[0];
            return all;
        }

        public object[] GetCustomAttributes(Type attributeType, bool inherit)
        {
            object[] all = GetCustomAttributes(inherit);
            int count = 0;
            for (int i = 0; i < all.Length; i++)
            {
                if (MemberInfo.MatchesFilter(all[i], attributeType)) count++;
            }
            object[] result = new object[count];
            int at = 0;
            for (int i = 0; i < all.Length; i++)
            {
                if (MemberInfo.MatchesFilter(all[i], attributeType))
                {
                    result[at] = all[i];
                    at = at + 1;
                }
            }
            return result;
        }
    }
}
