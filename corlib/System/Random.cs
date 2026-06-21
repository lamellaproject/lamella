// Lamella managed corlib (from scratch). -- System.Random
namespace System
{
    public class Random
    {
        private const int MBIG = 2147483647;
        private const int MSEED = 161803398;
        private const int MZ = 0;

        private int inext;
        private int inextp;
        private int[] SeedArray;

        public Random(int Seed)
        {
            SeedArray = new int[56];
            int subtraction = (Seed == -2147483648) ? 2147483647 : (Seed < 0 ? -Seed : Seed);
            int mj = MSEED - subtraction;
            SeedArray[55] = mj;
            int mk = 1;
            for (int i = 1; i < 55; i++)
            {
                int ii = (21 * i) % 55;
                SeedArray[ii] = mk;
                mk = mj - mk;
                if (mk < 0) mk = mk + MBIG;
                mj = SeedArray[ii];
            }
            for (int k = 1; k < 5; k++)
            {
                for (int i = 1; i < 56; i++)
                {
                    SeedArray[i] = SeedArray[i] - SeedArray[1 + (i + 30) % 55];
                    if (SeedArray[i] < 0) SeedArray[i] = SeedArray[i] + MBIG;
                }
            }
            inext = 0;
            inextp = 21;
        }

        public Random() : this(0) { }

        private int InternalSample()
        {
            int locINext = inext;
            int locINextp = inextp;
            locINext = locINext + 1;
            if (locINext >= 56) locINext = 1;
            locINextp = locINextp + 1;
            if (locINextp >= 56) locINextp = 1;
            int retVal = SeedArray[locINext] - SeedArray[locINextp];
            if (retVal == MBIG) retVal = retVal - 1;
            if (retVal < 0) retVal = retVal + MBIG;
            SeedArray[locINext] = retVal;
            inext = locINext;
            inextp = locINextp;
            return retVal;
        }

        protected virtual double Sample()
        {
            return InternalSample() * (1.0 / MBIG);
        }

        public virtual int Next()
        {
            return InternalSample();
        }

        public virtual int Next(int maxValue)
        {
            return (int)(Sample() * maxValue);
        }

        public virtual int Next(int minValue, int maxValue)
        {
            long range = (long)maxValue - (long)minValue;
            return (int)((long)(Sample() * (double)range) + (long)minValue);
        }

        public virtual double NextDouble()
        {
            return Sample();
        }
    }
}
