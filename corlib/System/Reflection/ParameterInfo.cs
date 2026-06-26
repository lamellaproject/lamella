// Lamella managed corlib (from scratch). -- System.Reflection.ParameterInfo
namespace System.Reflection
{
    public class ParameterInfo
    {
        private int _position;
        private Type _parameterType;
        private string _name;

        internal ParameterInfo(int position, Type parameterType, string name)
        {
            _position = position;
            _parameterType = parameterType;
            _name = name;
        }

        public int Position { get { return _position; } }
        public Type ParameterType { get { return _parameterType; } }
        public string Name { get { return _name; } }
    }
}
