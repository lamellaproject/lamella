// Lamella managed corlib (from scratch). -- System.AggregateException
#if LAMELLA_SURFACE_NETFX_4_0
namespace System
{
    /// <summary>Represents one or more errors that occurred together.</summary>
    public class AggregateException : Exception
    {
        private const string DefaultMessage = "One or more errors occurred.";
        private Exception[] _innerExceptions;

        /// <summary>An exception with the default message and no inner exceptions.</summary>
        public AggregateException() : base(DefaultMessage)
        {
            _innerExceptions = new Exception[0];
        }

        /// <summary>An exception with <paramref name="message"/> and no inner exceptions.</summary>
        /// <param name="message">The message.</param>
        public AggregateException(string message) : base(message)
        {
            _innerExceptions = new Exception[0];
        }

        /// <summary>An exception with <paramref name="message"/> carrying one inner exception.</summary>
        /// <param name="message">The message.</param>
        /// <param name="innerException">The exception it carries.</param>
        /// <exception cref="ArgumentNullException"><paramref name="innerException"/> is null.</exception>
        public AggregateException(string message, Exception innerException) : base(message, innerException)
        {
            if (innerException == null) throw new ArgumentNullException("innerException");
            _innerExceptions = new Exception[] { innerException };
        }

        /// <summary>An exception with the default message carrying <paramref name="innerExceptions"/>.</summary>
        /// <param name="innerExceptions">The exceptions it carries.</param>
        /// <exception cref="ArgumentNullException"><paramref name="innerExceptions"/> is null.</exception>
        /// <exception cref="ArgumentException">An element of <paramref name="innerExceptions"/> is null.</exception>
        public AggregateException(params Exception[] innerExceptions) : this(DefaultMessage, innerExceptions)
        {
        }

        /// <summary>An exception with <paramref name="message"/> carrying <paramref name="innerExceptions"/>.</summary>
        /// <param name="message">The message.</param>
        /// <param name="innerExceptions">The exceptions it carries.</param>
        /// <exception cref="ArgumentNullException"><paramref name="innerExceptions"/> is null.</exception>
        /// <exception cref="ArgumentException">An element of <paramref name="innerExceptions"/> is null.</exception>
        public AggregateException(string message, params Exception[] innerExceptions)
            : base(message, FirstOf(innerExceptions))
        {
            Exception[] copy = new Exception[innerExceptions.Length];
            for (int i = 0; i < copy.Length; i++)
            {
                if (innerExceptions[i] == null) throw new ArgumentException("An element of innerExceptions was null.");
                copy[i] = innerExceptions[i];
            }
            _innerExceptions = copy;
        }

        private static Exception FirstOf(Exception[] innerExceptions)
        {
            if (innerExceptions == null) throw new ArgumentNullException("innerExceptions");
            return innerExceptions.Length > 0 ? innerExceptions[0] : null;
        }

        /// <summary>The message, followed by each inner exception's message in parentheses.</summary>
        public override string Message
        {
            get
            {
                string message = base.Message;
                if (_innerExceptions.Length == 0) return message;
                for (int i = 0; i < _innerExceptions.Length; i++)
                {
                    message = message + " (" + _innerExceptions[i].Message + ")";
                }
                return message;
            }
        }

        /// <summary>The innermost exception reached through aggregates that each carry exactly one
        /// exception; this one when it carries none or several.</summary>
        /// <returns>That exception.</returns>
        public override Exception GetBaseException()
        {
            Exception back = this;
            AggregateException aggregate = this;
            while (aggregate != null && aggregate._innerExceptions.Length == 1)
            {
                back = back.InnerException;
                aggregate = back as AggregateException;
            }
            return back;
        }
    }
}
#endif
