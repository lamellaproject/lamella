// Lamella managed corlib (from scratch). -- System.Reflection.MemberInfo
namespace System.Reflection
{
    public class MemberInfo
    {
        protected MemberInfo() { }

#if LAMELLA_SURFACE_REFLECTION

        public virtual string Name
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }

        [Lamella.Runtime.RuntimeProvided] public virtual object[] GetCustomAttributes(bool inherit) { return null; }

        public virtual object[] GetCustomAttributes(Type attributeType, bool inherit)
        {
            object[] all = GetCustomAttributes(inherit);
            if (all == null) return new object[0];
            int count = 0;
            for (int i = 0; i < all.Length; i++)
            {
                if (MatchesFilter(all[i], attributeType)) count++;
            }
            object[] result = new object[count];
            int at = 0;
            for (int i = 0; i < all.Length; i++)
            {
                if (MatchesFilter(all[i], attributeType))
                {
                    result[at] = all[i];
                    at = at + 1;
                }
            }
            return result;
        }

        internal static bool MatchesFilter(object attribute, Type attributeType)
        {
            if (attribute == null || attributeType == null) return false;
            for (Type walk = attribute.GetType(); walk != null; walk = walk.BaseType)
            {
                if (walk == attributeType) return true;
            }
            return false;
        }
#endif
    }
}
