// Lamella managed corlib (from scratch). -- System.Globalization.CultureNotFoundException
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Globalization
{
    /// <summary>The exception thrown when a method is asked to construct a culture that is not
    /// available.</summary>
    public class CultureNotFoundException : ArgumentException
    {
        private const string DefaultMessage = "Culture is not supported.";
        private readonly string _invalidCultureName;
        private readonly int? _invalidCultureId;

        private static string Text(string message)
        {
            return message == null ? DefaultMessage : message;
        }

        /// <summary>Initializes the exception with the default message.</summary>
        public CultureNotFoundException() : base(DefaultMessage)
        {
        }

        /// <summary>Initializes the exception with a message.</summary>
        /// <param name="message">The message.</param>
        public CultureNotFoundException(string message) : base(Text(message))
        {
        }

        /// <summary>Initializes the exception with the parameter at fault and a message.</summary>
        /// <param name="paramName">The name of the parameter.</param>
        /// <param name="message">The message.</param>
        public CultureNotFoundException(string paramName, string message) : base(Text(message), paramName)
        {
        }

        /// <summary>Initializes the exception with a message and the exception that caused it.</summary>
        /// <param name="message">The message.</param>
        /// <param name="innerException">The exception that caused this one.</param>
        public CultureNotFoundException(string message, Exception innerException) : base(Text(message), innerException)
        {
        }

        /// <summary>Initializes the exception with the parameter at fault, the culture name that is not
        /// available, and a message.</summary>
        /// <param name="paramName">The name of the parameter.</param>
        /// <param name="invalidCultureName">The culture name.</param>
        /// <param name="message">The message.</param>
        public CultureNotFoundException(string paramName, string invalidCultureName, string message) : base(Text(message), paramName)
        {
            _invalidCultureName = invalidCultureName;
        }

        /// <summary>Initializes the exception with a message, the culture name that is not available,
        /// and the exception that caused it.</summary>
        /// <param name="message">The message.</param>
        /// <param name="invalidCultureName">The culture name.</param>
        /// <param name="innerException">The exception that caused this one.</param>
        public CultureNotFoundException(string message, string invalidCultureName, Exception innerException) : base(Text(message), innerException)
        {
            _invalidCultureName = invalidCultureName;
        }

        /// <summary>Initializes the exception with the parameter at fault, the culture identifier that is
        /// not available, and a message.</summary>
        /// <param name="paramName">The name of the parameter.</param>
        /// <param name="invalidCultureId">The culture identifier.</param>
        /// <param name="message">The message.</param>
        public CultureNotFoundException(string paramName, int invalidCultureId, string message) : base(Text(message), paramName)
        {
            _invalidCultureId = invalidCultureId;
        }

        /// <summary>Initializes the exception with a message, the culture identifier that is not
        /// available, and the exception that caused it.</summary>
        /// <param name="message">The message.</param>
        /// <param name="invalidCultureId">The culture identifier.</param>
        /// <param name="innerException">The exception that caused this one.</param>
        public CultureNotFoundException(string message, int invalidCultureId, Exception innerException) : base(Text(message), innerException)
        {
            _invalidCultureId = invalidCultureId;
        }

        /// <summary>The culture identifier that is not available, if one was given.</summary>
        public virtual int? InvalidCultureId
        {
            get { return _invalidCultureId; }
        }

        /// <summary>The culture name that is not available, if one was given.</summary>
        public virtual string InvalidCultureName
        {
            get { return _invalidCultureName; }
        }

        /// <summary>The message, with the culture that is not available on a line of its own.</summary>
        public override string Message
        {
            get
            {
                string message = base.Message;
                if (_invalidCultureId == null && _invalidCultureName == null) { return message; }
                string culture = _invalidCultureName;
                if (_invalidCultureId != null)
                {
                    int id = _invalidCultureId.Value;
                    culture = id.ToString("D") + " (0x" + id.ToString("x4") + ")";
                }
                return message + Environment.NewLine + culture + " is an invalid culture identifier.";
            }
        }
    }
}
#endif
