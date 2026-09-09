// Lamella.Hardware -- the pin-change event registry, in the System.Device.Gpio assembly.
using System;
using System.Device.Gpio;

namespace Lamella.Hardware
{
    /// <summary>Routes pin-change interrupts to the handlers registered for them.</summary>
    public sealed class PinEvents
    {
        private PinEvents() { }

        private static int[] tokens;
        private static int[] pins;
        private static PinEventTypes[] wanted;
        private static PinChangeEventHandler[] handlers;
        private static object[] senders;
        private static int count;

        private static int lost;

        /// <summary>How many times the queue between the interrupt handler and this class has
        /// dropped events because it was full.</summary>
        /// <remarks>
        /// Losing an edge under a burst is permitted -- the GPIO surface does not promise every edge
        /// is delivered -- but losing one silently is a different claim. A handler that sees three
        /// edges where five happened cannot tell that from three edges having happened, and nothing
        /// else in the program can tell it either. This count is what makes the difference
        /// answerable. It counts DRAINS that lost something rather than individual events, because a
        /// burst that overflowed is one gap in the sequence and not one gap per surviving event.
        /// </remarks>
        public static int LostEvents
        {
            get { return lost; }
        }

        /// <summary>Registers <paramref name="callback"/> to be invoked when the line the board
        /// identifies by <paramref name="token"/> reports an edge in
        /// <paramref name="eventTypes"/>.</summary>
        /// <param name="sender">Passed to the callback as the event's sender, normally the driver.</param>
        /// <param name="token">The board's own identifier for the armed line, carried whole and
        /// never interpreted here.</param>
        /// <param name="pinNumber">The pin as the caller numbered it, reported back on the event
        /// arguments.</param>
        /// <param name="eventTypes">Which edges this callback wants.</param>
        /// <param name="callback">The handler to invoke.</param>
        public static void Register(
            object sender, int token, int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
        {
            if ((object)callback == null)
            {
                throw new ArgumentNullException("callback");
            }
            for (int i = 0; i < count; i = i + 1)
            {
                if (tokens[i] == token && pins[i] == pinNumber && wanted[i] == eventTypes
                    && (object)senders[i] == (object)sender)
                {
                    handlers[i] = (PinChangeEventHandler)Delegate.Combine(handlers[i], callback);
                    return;
                }
            }
            Grow(count + 1);
            tokens[count] = token;
            pins[count] = pinNumber;
            wanted[count] = eventTypes;
            handlers[count] = callback;
            senders[count] = sender;
            count = count + 1;
        }

        /// <summary>Removes <paramref name="callback"/> from every registration made for
        /// <paramref name="token"/>. Does nothing if there is none.</summary>
        public static void Unregister(int token, PinChangeEventHandler callback)
        {
            for (int i = count - 1; i >= 0; i = i - 1)
            {
                if (tokens[i] != token)
                {
                    continue;
                }
                handlers[i] = (PinChangeEventHandler)Delegate.Remove(handlers[i], callback);
                if ((object)handlers[i] == null)
                {
                    int last = count - 1;
                    tokens[i] = tokens[last];
                    pins[i] = pins[last];
                    wanted[i] = wanted[last];
                    handlers[i] = handlers[last];
                    senders[i] = senders[last];
                    handlers[last] = null;
                    senders[last] = null;
                    count = last;
                }
            }
        }

        internal static void Dispatch(int token, bool level)
        {
            PinEventTypes changeType = level ? PinEventTypes.Rising : PinEventTypes.Falling;
            for (int i = 0; i < count; i = i + 1)
            {
                if (tokens[i] != token)
                {
                    continue;
                }
                if ((wanted[i] & changeType) == PinEventTypes.None)
                {
                    continue;
                }
                handlers[i](senders[i], new PinValueChangedEventArgs(changeType, pins[i]));
            }
        }

        internal static void ReportLost()
        {
            lost = lost + 1;
        }

        private static void Grow(int needed)
        {
            int capacity = tokens == null ? 0 : tokens.Length;
            if (capacity >= needed)
            {
                return;
            }
            int grown = capacity == 0 ? 4 : capacity * 2;
            while (grown < needed)
            {
                grown = grown * 2;
            }
            int[] grownTokens = new int[grown];
            int[] grownPins = new int[grown];
            PinEventTypes[] grownWanted = new PinEventTypes[grown];
            PinChangeEventHandler[] grownHandlers = new PinChangeEventHandler[grown];
            object[] grownSenders = new object[grown];
            for (int i = 0; i < count; i = i + 1)
            {
                grownTokens[i] = tokens[i];
                grownPins[i] = pins[i];
                grownWanted[i] = wanted[i];
                grownHandlers[i] = handlers[i];
                grownSenders[i] = senders[i];
            }
            tokens = grownTokens;
            pins = grownPins;
            wanted = grownWanted;
            handlers = grownHandlers;
            senders = grownSenders;
        }
    }
}
