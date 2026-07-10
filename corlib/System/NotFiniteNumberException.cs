// Lamella managed corlib (from scratch). -- System.NotFiniteNumberException
namespace System
{
    public class NotFiniteNumberException : ArithmeticException
    {
        private double _offendingNumber;

        public NotFiniteNumberException() : base() { }
        public NotFiniteNumberException(double offendingNumber) : base() { _offendingNumber = offendingNumber; }
        public NotFiniteNumberException(string message) : base(message) { }
        public NotFiniteNumberException(string message, double offendingNumber) : base(message) { _offendingNumber = offendingNumber; }
        public NotFiniteNumberException(string message, Exception innerException) : base(message, innerException) { }
        public NotFiniteNumberException(string message, double offendingNumber, Exception innerException) : base(message, innerException) { _offendingNumber = offendingNumber; }

        public double OffendingNumber { get { return _offendingNumber; } }
    }
}
