// Lamella managed corlib (from scratch). -- System.Collections.StackEnumerator
namespace System.Collections
{
    internal class StackEnumerator : IEnumerator, IDisposable
    {
        private Stack stack;
        private int index;
        private int version;
        private object current;

        public StackEnumerator(Stack stack)
        {
            this.stack = stack;
            this.version = stack.version;
            this.index = -2;
        }

        public bool MoveNext()
        {
            CheckVersion();
            if (index == -2)
            {
                index = stack.Count - 1;
                if (index < 0) return false;
                current = stack.GetElement(index);
                return true;
            }
            if (index == -1) return false;
            index = index - 1;
            if (index < 0)
            {
                current = null;
                return false;
            }
            current = stack.GetElement(index);
            return true;
        }

        public object Current
        {
            get
            {
                if (index == -2) throw new InvalidOperationException("Enumeration has not started. Call MoveNext.");
                if (index == -1) throw new InvalidOperationException("Enumeration already finished.");
                return current;
            }
        }

        public void Reset()
        {
            CheckVersion();
            index = -2;
            current = null;
        }

        private void CheckVersion()
        {
            if (version != stack.version)
            {
                throw new InvalidOperationException("Collection was modified after the enumerator was instantiated.");
            }
        }

        public void Dispose() { }
    }
}
