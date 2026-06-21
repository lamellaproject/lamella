// Lamella managed corlib (from scratch). -- System.Collections.StackEnumerator
namespace System.Collections
{
    internal class StackEnumerator : IEnumerator, IDisposable
    {
        private Stack stack;
        private int index;

        public StackEnumerator(Stack stack)
        {
            this.stack = stack;
            this.index = stack.Count;
        }

        public bool MoveNext()
        {
            index = index - 1;
            return index >= 0;
        }

        public object Current
        {
            get { return stack.GetElement(index); }
        }

        public void Reset() { index = stack.Count; }

        public void Dispose() { }
    }
}
