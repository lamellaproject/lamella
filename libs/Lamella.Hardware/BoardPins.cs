// Lamella.Hardware.BoardPins -- the board's pins, by number and by the labels the board prints.
using System.Collections;
using System.Collections.Generic;

namespace Lamella.Hardware
{
    /// <summary>The board's pins, by number and by the labels the board prints.</summary>
    public sealed class BoardPins : IReadOnlyList<BoardPin>
    {
        private readonly BoardPin[] _pins;

        /// <summary>The pins <paramref name="pins"/> lists, which must be in pin-number order: the
        /// record at index n carries pin n.</summary>
        /// <param name="pins">The pins' records, in pin-number order.</param>
        /// <exception cref="System.ArgumentNullException"><paramref name="pins"/> is null.</exception>
        /// <exception cref="System.ArgumentException">A record is null, or a record's pin number is not
        /// its index.</exception>
        public BoardPins(IReadOnlyList<BoardPin> pins)
        {
            if ((object)pins == null) throw new System.ArgumentNullException("pins");
            BoardPin[] copied = new BoardPin[pins.Count];
            for (int index = 0; index < copied.Length; index++)
            {
                BoardPin pin = pins[index];
                if ((object)pin == null) throw new System.ArgumentException("a pin record is null", "pins");
                if (pin.PinNumber != index)
                {
                    throw new System.ArgumentException("the pin records are not in pin-number order: record "
                        + FactsTable.Decimal(index) + " carries pin " + FactsTable.Decimal(pin.PinNumber), "pins");
                }
                copied[index] = pin;
            }
            _pins = copied;
        }

        /// <summary>The pins of the board the program was built for, from its facts.</summary>
        internal BoardPins()
        {
        }

        /// <summary>How many pins the board numbers.</summary>
        public int Count
        {
            get { return (object)_pins == null ? FactsTable.PinCount : _pins.Length; }
        }

        /// <summary>The record of pin <paramref name="pinNumber"/>.</summary>
        /// <param name="pinNumber">The pin's number, the one <c>GpioController.OpenPin</c> takes.</param>
        /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> is not one
        /// of the board's pins.</exception>
        public BoardPin this[int pinNumber]
        {
            get
            {
                int count = Count;
                if (pinNumber < 0 || pinNumber >= count)
                {
                    throw new System.ArgumentOutOfRangeException("pinNumber", pinNumber,
                        "the board has pins 0 to " + FactsTable.Decimal(count - 1));
                }
                return (object)_pins == null ? FactsTable.Pin(pinNumber) : _pins[pinNumber];
            }
        }

        /// <summary>The record of the first pin that carries <paramref name="label"/>, compared
        /// ordinally ("GP25"), or null where no pin carries it.</summary>
        /// <param name="label">A label the board prints.</param>
        /// <exception cref="System.ArgumentNullException"><paramref name="label"/> is null.</exception>
        public BoardPin this[string label]
        {
            get
            {
                if ((object)label == null) throw new System.ArgumentNullException("label");
                if ((object)_pins == null)
                {
                    int number = FactsTable.FindLabel(label);
                    return number < 0 ? null : FactsTable.Pin(number);
                }
                for (int index = 0; index < _pins.Length; index++)
                {
                    IReadOnlyList<string> labels = _pins[index].Labels;
                    for (int at = 0; at < labels.Count; at++)
                    {
                        if (string.Equals(labels[at], label)) return _pins[index];
                    }
                }
                return null;
            }
        }

        /// <summary>Walks the pins in pin-number order.</summary>
        public IEnumerator<BoardPin> GetEnumerator()
        {
            return new PinWalker(All());
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return new PinWalker(All());
        }

        /// <summary>Every pin's record, by pin number.</summary>
        private BoardPin[] All()
        {
            return (object)_pins == null ? FactsTable.AllPins() : _pins;
        }
    }
}
