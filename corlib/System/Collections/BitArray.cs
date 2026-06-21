// Lamella managed corlib (from scratch). -- System.Collections.BitArray
namespace System.Collections
{
    public class BitArray
    {
        private int[] bits;
        private int length;

        public BitArray(int length)
        {
            this.length = length;
            bits = new int[(length + 31) / 32];
        }

        public int Length { get { return length; } }
        public int Count { get { return length; } }

        public bool Get(int index)
        {
            return (bits[index / 32] & (1 << (index % 32))) != 0;
        }

        public void Set(int index, bool value)
        {
            int word = index / 32;
            int mask = 1 << (index % 32);
            if (value) bits[word] = bits[word] | mask;
            else bits[word] = bits[word] & ~mask;
        }
    }
}
