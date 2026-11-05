// Lamella.Hardware.FactsTable -- the board's pin and bus records, made from its facts assembly's tables when a program first asks for one.
using System.Collections;
using System.Collections.Generic;
using Lamella.Generated;

namespace Lamella.Hardware
{
    /// <summary>Makes the board's pin and bus records from its facts, each once, when first asked
    /// for.</summary>
    internal sealed class FactsTable
    {
        private FactsTable() { }

        /// <summary>How many chars of the pin table each pin takes.</summary>
        private const int PinStride = 6;

        /// <summary>What a walk's <c>Current</c> says before the walk begins or after it ends.</summary>
        internal const string NotWalking = "The enumeration has not started or has already finished.";

        private static BoardPin[] s_pins;
        private static I2cBusList s_i2cBuses;
        private static SpiBusList s_spiBuses;

        /// <summary>How many pins the board's table numbers.</summary>
        internal static int PinCount
        {
            get { return TargetBoardFacts.PinCount; }
        }

        /// <summary>The records made so far, by pin number, null where a pin's is not yet made.</summary>
        private static BoardPin[] Made()
        {
            BoardPin[] made = s_pins;
            if ((object)made == null)
            {
                made = new BoardPin[PinCount];
                s_pins = made;
            }
            return made;
        }

        /// <summary>Every pin's record, by pin number, each made now where it was not yet.</summary>
        internal static BoardPin[] AllPins()
        {
            BoardPin[] made = Made();
            for (int number = 0; number < made.Length; number++) Pin(number);
            return made;
        }

        /// <summary>The record of pin <paramref name="number"/>, which the caller has checked is one of
        /// the board's.</summary>
        internal static BoardPin Pin(int number)
        {
            BoardPin[] made = Made();
            BoardPin pin = made[number];
            if ((object)pin == null)
            {
                string table = TargetBoardFacts.Pins;
                int at = number * PinStride;
                pin = new BoardPin(number, LabelsOf(number), (PinFunctions)table[at], Stored(table[at + 1]),
                    Stored(table[at + 2]), Stored(table[at + 3]), Stored(table[at + 4]), Stored(table[at + 5]));
                made[number] = pin;
            }
            return pin;
        }

        /// <summary><paramref name="value"/> folded into <paramref name="hash"/>, by the multiplier a
        /// record's generated hash folds its members with.</summary>
        internal static int FoldHash(int hash, int value)
        {
            return hash * -1521134295 + value;
        }

        /// <summary><paramref name="value"/> in decimal digits, led by a minus sign where it is
        /// negative.</summary>
        internal static string Decimal(int value)
        {
            if (value == 0) return "0";
            bool negative = value < 0;
            int rest = negative ? value : -value;
            char[] digits = new char[11];
            int at = digits.Length;
            while (rest != 0)
            {
                at--;
                digits[at] = (char)('0' - rest % 10);
                rest = rest / 10;
            }
            if (negative)
            {
                at--;
                digits[at] = '-';
            }
            return new string(digits, at, digits.Length - at);
        }

        /// <summary>A number stored plus one, or null where 0 is stored.</summary>
        private static int? Stored(char stored)
        {
            if (stored == '\0') return null;
            return stored - 1;
        }

        /// <summary>The PWM chip of pin <paramref name="number"/>, stored plus one: 0 where no channel
        /// drives it.</summary>
        internal static int StoredPwmChip(int number)
        {
            return TargetBoardFacts.Pins[number * PinStride + 3];
        }

        /// <summary>The PWM channel of pin <paramref name="number"/>, stored plus one: 0 where no
        /// channel drives it.</summary>
        internal static int StoredPwmChannel(int number)
        {
            return TargetBoardFacts.Pins[number * PinStride + 4];
        }

        /// <summary>Where pin <paramref name="number"/>'s labels begin in the label table.</summary>
        private static int LabelStart(string labels, int number)
        {
            int at = 0;
            for (int pin = 0; pin < number; pin++)
            {
                at = labels.IndexOf('\n', at) + 1;
            }
            return at;
        }

        /// <summary>Where the labels that begin at <paramref name="start"/> end: the next '\n', or the
        /// table's end.</summary>
        private static int LabelEnd(string labels, int start)
        {
            int end = labels.IndexOf('\n', start);
            return end < 0 ? labels.Length : end;
        }

        /// <summary>Pin <paramref name="number"/>'s labels, in the board's order.</summary>
        private static LabelList LabelsOf(int number)
        {
            string labels = TargetBoardFacts.Labels;
            int start = LabelStart(labels, number);
            int end = LabelEnd(labels, start);
            int count = 0;
            for (int at = start; at < end; at++)
            {
                if (at == start || labels[at - 1] == ' ') count++;
            }
            string[] each = new string[count];
            int next = 0;
            int from = start;
            for (int at = start; at <= end; at++)
            {
                if (at == end || labels[at] == ' ')
                {
                    if (at > from) each[next++] = labels.Substring(from, at - from);
                    from = at + 1;
                }
            }
            return new LabelList(each);
        }

        /// <summary>The first label pin <paramref name="number"/> carries, or "pin N" where it carries
        /// none.</summary>
        internal static string FirstLabel(int number)
        {
            string labels = TargetBoardFacts.Labels;
            int start = LabelStart(labels, number);
            int end = LabelEnd(labels, start);
            int space = labels.IndexOf(' ', start);
            if (space >= 0 && space < end) end = space;
            if (end == start) return "pin " + Decimal(number);
            return labels.Substring(start, end - start);
        }

        /// <summary>The number of the first pin that carries <paramref name="label"/>, compared
        /// ordinally, or -1 where none does.</summary>
        internal static int FindLabel(string label)
        {
            if (label.Length == 0) return -1;
            string labels = TargetBoardFacts.Labels;
            int pin = 0;
            int from = 0;
            for (int at = 0; at <= labels.Length; at++)
            {
                bool last = at == labels.Length;
                char c = last ? '\n' : labels[at];
                if (c != ' ' && c != '\n') continue;
                if (at - from == label.Length && Matches(labels, from, label)) return pin;
                from = at + 1;
                if (c == '\n') pin++;
            }
            return -1;
        }

        /// <summary>Whether <paramref name="label"/> is written in <paramref name="labels"/> at
        /// <paramref name="at"/>, char for char.</summary>
        private static bool Matches(string labels, int at, string label)
        {
            for (int index = 0; index < label.Length; index++)
            {
                if (labels[at + index] != label[index]) return false;
            }
            return true;
        }

        /// <summary>The I2C buses the image serves, each made when first read.</summary>
        internal static IReadOnlyList<I2cBusInfo> I2cBuses
        {
            get
            {
                I2cBusList buses = s_i2cBuses;
                if ((object)buses == null)
                {
                    buses = new I2cBusList(ServedI2c());
                    s_i2cBuses = buses;
                }
                return buses;
            }
        }

        /// <summary>The SPI buses the image serves, each made when first read.</summary>
        internal static IReadOnlyList<SpiBusInfo> SpiBuses
        {
            get
            {
                SpiBusList buses = s_spiBuses;
                if ((object)buses == null)
                {
                    buses = new SpiBusList(ServedSpi());
                    s_spiBuses = buses;
                }
                return buses;
            }
        }

        /// <summary>Where each I2C bus the image serves begins in the I2C table, in table
        /// order.</summary>
        internal static int[] ServedI2c()
        {
            string table = TargetBoardFacts.I2cBuses;
            int count = 0;
            for (int at = 0; at < table.Length; at += 3)
            {
                if (Buses.ServesI2c(table[at])) count++;
            }
            int[] served = new int[count];
            int next = 0;
            for (int at = 0; at < table.Length; at += 3)
            {
                if (Buses.ServesI2c(table[at])) served[next++] = at;
            }
            return served;
        }

        /// <summary>Where each SPI bus the image serves begins in the SPI table, in table
        /// order.</summary>
        internal static int[] ServedSpi()
        {
            string table = TargetBoardFacts.SpiBuses;
            int count = 0;
            for (int at = 0; at < table.Length; at = NextSpi(table, at))
            {
                if (Buses.ServesSpi(table[at])) count++;
            }
            int[] served = new int[count];
            int next = 0;
            for (int at = 0; at < table.Length; at = NextSpi(table, at))
            {
                if (Buses.ServesSpi(table[at])) served[next++] = at;
            }
            return served;
        }

        /// <summary>The numbers of the I2C buses the image serves, in table order and separated by
        /// commas, or "none".</summary>
        internal static string ServedI2cNumbers()
        {
            string table = TargetBoardFacts.I2cBuses;
            string listed = null;
            for (int at = 0; at < table.Length; at += 3)
            {
                if (!Buses.ServesI2c(table[at])) continue;
                string number = Decimal(table[at]);
                listed = (object)listed == null ? number : listed + ", " + number;
            }
            return (object)listed == null ? "none" : listed;
        }

        /// <summary>The numbers of the SPI buses the image serves, in table order and separated by
        /// commas, or "none".</summary>
        internal static string ServedSpiNumbers()
        {
            string table = TargetBoardFacts.SpiBuses;
            string listed = null;
            for (int at = 0; at < table.Length; at = NextSpi(table, at))
            {
                if (!Buses.ServesSpi(table[at])) continue;
                string number = Decimal(table[at]);
                listed = (object)listed == null ? number : listed + ", " + number;
            }
            return (object)listed == null ? "none" : listed;
        }

        /// <summary>Where the SPI bus after the one at <paramref name="at"/> begins.</summary>
        private static int NextSpi(string table, int at)
        {
            return at + 5 + table[at + 4] + 4;
        }

        /// <summary>The I2C bus whose entry begins at <paramref name="at"/>, made now.</summary>
        internal static I2cBusInfo MakeI2c(int at)
        {
            string table = TargetBoardFacts.I2cBuses;
            int number = table[at];
            I2cDriver driver = Buses.CreatedI2c(number);
            int[] rates = (object)driver == null ? null : driver.Rates;
            int[] frequencies;
            if ((object)rates == null)
            {
                frequencies = new int[] { TargetBoard.StandardI2cFrequency };
            }
            else
            {
                frequencies = new int[rates.Length];
                for (int index = 0; index < rates.Length; index++) frequencies[index] = rates[index];
            }
            return new I2cBusInfo(number, PinOrNull(table[at + 1]), PinOrNull(table[at + 2]), new RateList(frequencies));
        }

        /// <summary>The SPI bus whose entry begins at <paramref name="at"/>, made now.</summary>
        internal static SpiBusInfo MakeSpi(int at)
        {
            string table = TargetBoardFacts.SpiBuses;
            int selects = table[at + 4];
            BoardPin[] chipSelects = new BoardPin[selects];
            for (int index = 0; index < selects; index++) chipSelects[index] = PinOrNull(table[at + 5 + index]);
            int rates = at + 5 + selects;
            return new SpiBusInfo(table[at], PinOrNull(table[at + 1]), PinOrNull(table[at + 2]), PinOrNull(table[at + 3]),
                new PinList(chipSelects), Wide(table, rates), Wide(table, rates + 2));
        }

        /// <summary>The pin a bus table stores plus one, or null where it stores 0.</summary>
        private static BoardPin PinOrNull(char stored)
        {
            if (stored == '\0') return null;
            return Pin(stored - 1);
        }

        /// <summary>The number stored at <paramref name="at"/> as two chars of 15 bits, high
        /// first.</summary>
        private static int Wide(string table, int at)
        {
            return (table[at] << 15) | table[at + 1];
        }
    }


    /// <summary>The I2C buses the image serves, in the board's order, each made when first
    /// read.</summary>
    internal sealed class I2cBusList : IReadOnlyList<I2cBusInfo>
    {
        private readonly int[] _entries;
        private readonly I2cBusInfo[] _made;

        internal I2cBusList(int[] entries)
        {
            _entries = entries;
            _made = new I2cBusInfo[entries.Length];
        }

        /// <summary>How many buses the image serves.</summary>
        public int Count
        {
            get { return _entries.Length; }
        }

        /// <summary>The bus at <paramref name="index"/> in the board's order.</summary>
        public I2cBusInfo this[int index]
        {
            get
            {
                if ((uint)index >= (uint)_entries.Length) throw new System.ArgumentOutOfRangeException("index");
                return Made(index);
            }
        }

        /// <summary>Walks the buses in order.</summary>
        public IEnumerator<I2cBusInfo> GetEnumerator()
        {
            return new I2cBusWalker(All());
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return new I2cBusWalker(All());
        }

        /// <summary>The name of the array this list stands for, "Lamella.Hardware.I2cBusInfo[]", which is what a record
        /// holding it prints.</summary>
        public override string ToString()
        {
            return "Lamella.Hardware.I2cBusInfo[]";
        }

        /// <summary>The bus at <paramref name="index"/>, made now where it was not yet.</summary>
        private I2cBusInfo Made(int index)
        {
            I2cBusInfo bus = _made[index];
            if ((object)bus == null)
            {
                bus = FactsTable.MakeI2c(_entries[index]);
                _made[index] = bus;
            }
            return bus;
        }

        /// <summary>Every bus, in the board's order, each made now where it was not yet.</summary>
        private I2cBusInfo[] All()
        {
            for (int index = 0; index < _made.Length; index++) Made(index);
            return _made;
        }
    }

    /// <summary>The SPI buses the image serves, in the board's order, each made when first
    /// read.</summary>
    internal sealed class SpiBusList : IReadOnlyList<SpiBusInfo>
    {
        private readonly int[] _entries;
        private readonly SpiBusInfo[] _made;

        internal SpiBusList(int[] entries)
        {
            _entries = entries;
            _made = new SpiBusInfo[entries.Length];
        }

        /// <summary>How many buses the image serves.</summary>
        public int Count
        {
            get { return _entries.Length; }
        }

        /// <summary>The bus at <paramref name="index"/> in the board's order.</summary>
        public SpiBusInfo this[int index]
        {
            get
            {
                if ((uint)index >= (uint)_entries.Length) throw new System.ArgumentOutOfRangeException("index");
                return Made(index);
            }
        }

        /// <summary>Walks the buses in order.</summary>
        public IEnumerator<SpiBusInfo> GetEnumerator()
        {
            return new SpiBusWalker(All());
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return new SpiBusWalker(All());
        }

        /// <summary>The name of the array this list stands for, "Lamella.Hardware.SpiBusInfo[]", which is what a record
        /// holding it prints.</summary>
        public override string ToString()
        {
            return "Lamella.Hardware.SpiBusInfo[]";
        }

        /// <summary>The bus at <paramref name="index"/>, made now where it was not yet.</summary>
        private SpiBusInfo Made(int index)
        {
            SpiBusInfo bus = _made[index];
            if ((object)bus == null)
            {
                bus = FactsTable.MakeSpi(_entries[index]);
                _made[index] = bus;
            }
            return bus;
        }

        /// <summary>Every bus, in the board's order, each made now where it was not yet.</summary>
        private SpiBusInfo[] All()
        {
            for (int index = 0; index < _made.Length; index++) Made(index);
            return _made;
        }
    }

    /// <summary>A pin's labels, over an array no caller can reach, so the list never changes.</summary>
    internal sealed class LabelList : IReadOnlyList<string>
    {
        private readonly string[] _items;

        internal LabelList(string[] items)
        {
            _items = items;
        }

        /// <summary>How many labels the list holds.</summary>
        public int Count
        {
            get { return _items.Length; }
        }

        /// <summary>The label at <paramref name="index"/>.</summary>
        public string this[int index]
        {
            get
            {
                if ((uint)index >= (uint)_items.Length) throw new System.ArgumentOutOfRangeException("index");
                return _items[index];
            }
        }

        /// <summary>Walks the labels in order.</summary>
        public IEnumerator<string> GetEnumerator()
        {
            return new LabelWalker(_items);
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return new LabelWalker(_items);
        }

        /// <summary>The name of the array this list stands for, "System.String[]", which is what a record
        /// holding it prints.</summary>
        public override string ToString()
        {
            return "System.String[]";
        }
    }

    /// <summary>A bus's rates in hertz, over an array no caller can reach, so the list never changes.</summary>
    internal sealed class RateList : IReadOnlyList<int>
    {
        private readonly int[] _items;

        internal RateList(int[] items)
        {
            _items = items;
        }

        /// <summary>How many rates the list holds.</summary>
        public int Count
        {
            get { return _items.Length; }
        }

        /// <summary>The rate at <paramref name="index"/>.</summary>
        public int this[int index]
        {
            get
            {
                if ((uint)index >= (uint)_items.Length) throw new System.ArgumentOutOfRangeException("index");
                return _items[index];
            }
        }

        /// <summary>Walks the rates in order.</summary>
        public IEnumerator<int> GetEnumerator()
        {
            return new RateWalker(_items);
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return new RateWalker(_items);
        }

        /// <summary>The name of the array this list stands for, "System.Int32[]", which is what a record
        /// holding it prints.</summary>
        public override string ToString()
        {
            return "System.Int32[]";
        }
    }

    /// <summary>Pins a bus names, over an array no caller can reach, so the list never changes.</summary>
    internal sealed class PinList : IReadOnlyList<BoardPin>
    {
        private readonly BoardPin[] _items;

        internal PinList(BoardPin[] items)
        {
            _items = items;
        }

        /// <summary>How many pins the list holds.</summary>
        public int Count
        {
            get { return _items.Length; }
        }

        /// <summary>The pin at <paramref name="index"/>.</summary>
        public BoardPin this[int index]
        {
            get
            {
                if ((uint)index >= (uint)_items.Length) throw new System.ArgumentOutOfRangeException("index");
                return _items[index];
            }
        }

        /// <summary>Walks the pins in order.</summary>
        public IEnumerator<BoardPin> GetEnumerator()
        {
            return new PinWalker(_items);
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return new PinWalker(_items);
        }

        /// <summary>The name of the array this list stands for, "Lamella.Hardware.BoardPin[]", which is what a record
        /// holding it prints.</summary>
        public override string ToString()
        {
            return "Lamella.Hardware.BoardPin[]";
        }
    }

    /// <summary>Walks labels in order.</summary>
    internal sealed class LabelWalker : IEnumerator<string>
    {
        private readonly string[] _items;
        private int _index;

        internal LabelWalker(string[] items)
        {
            _items = items;
            _index = -1;
        }

        /// <summary>The label the walk is at.</summary>
        /// <exception cref="System.InvalidOperationException">The walk has not begun, or has
        /// ended.</exception>
        public string Current
        {
            get
            {
                if (_index < 0 || _index >= _items.Length) throw new System.InvalidOperationException(FactsTable.NotWalking);
                return _items[_index];
            }
        }

        object IEnumerator.Current
        {
            get { return Current; }
        }

        /// <summary>Moves to the next label, answering false past the last.</summary>
        public bool MoveNext()
        {
            if (_index < _items.Length) _index++;
            return _index < _items.Length;
        }

        /// <summary>Moves back to before the first label.</summary>
        public void Reset()
        {
            _index = -1;
        }

        /// <summary>Releases nothing: the walk holds nothing to release.</summary>
        public void Dispose()
        {
        }
    }

    /// <summary>Walks rates in order.</summary>
    internal sealed class RateWalker : IEnumerator<int>
    {
        private readonly int[] _items;
        private int _index;

        internal RateWalker(int[] items)
        {
            _items = items;
            _index = -1;
        }

        /// <summary>The rate the walk is at.</summary>
        /// <exception cref="System.InvalidOperationException">The walk has not begun, or has
        /// ended.</exception>
        public int Current
        {
            get
            {
                if (_index < 0 || _index >= _items.Length) throw new System.InvalidOperationException(FactsTable.NotWalking);
                return _items[_index];
            }
        }

        object IEnumerator.Current
        {
            get { return Current; }
        }

        /// <summary>Moves to the next rate, answering false past the last.</summary>
        public bool MoveNext()
        {
            if (_index < _items.Length) _index++;
            return _index < _items.Length;
        }

        /// <summary>Moves back to before the first rate.</summary>
        public void Reset()
        {
            _index = -1;
        }

        /// <summary>Releases nothing: the walk holds nothing to release.</summary>
        public void Dispose()
        {
        }
    }

    /// <summary>Walks pins in order.</summary>
    internal sealed class PinWalker : IEnumerator<BoardPin>
    {
        private readonly BoardPin[] _items;
        private int _index;

        internal PinWalker(BoardPin[] items)
        {
            _items = items;
            _index = -1;
        }

        /// <summary>The pin the walk is at.</summary>
        /// <exception cref="System.InvalidOperationException">The walk has not begun, or has
        /// ended.</exception>
        public BoardPin Current
        {
            get
            {
                if (_index < 0 || _index >= _items.Length) throw new System.InvalidOperationException(FactsTable.NotWalking);
                return _items[_index];
            }
        }

        object IEnumerator.Current
        {
            get { return Current; }
        }

        /// <summary>Moves to the next pin, answering false past the last.</summary>
        public bool MoveNext()
        {
            if (_index < _items.Length) _index++;
            return _index < _items.Length;
        }

        /// <summary>Moves back to before the first pin.</summary>
        public void Reset()
        {
            _index = -1;
        }

        /// <summary>Releases nothing: the walk holds nothing to release.</summary>
        public void Dispose()
        {
        }
    }

    /// <summary>Walks I2C buses in order.</summary>
    internal sealed class I2cBusWalker : IEnumerator<I2cBusInfo>
    {
        private readonly I2cBusInfo[] _items;
        private int _index;

        internal I2cBusWalker(I2cBusInfo[] items)
        {
            _items = items;
            _index = -1;
        }

        /// <summary>The bus the walk is at.</summary>
        /// <exception cref="System.InvalidOperationException">The walk has not begun, or has
        /// ended.</exception>
        public I2cBusInfo Current
        {
            get
            {
                if (_index < 0 || _index >= _items.Length) throw new System.InvalidOperationException(FactsTable.NotWalking);
                return _items[_index];
            }
        }

        object IEnumerator.Current
        {
            get { return Current; }
        }

        /// <summary>Moves to the next bus, answering false past the last.</summary>
        public bool MoveNext()
        {
            if (_index < _items.Length) _index++;
            return _index < _items.Length;
        }

        /// <summary>Moves back to before the first bus.</summary>
        public void Reset()
        {
            _index = -1;
        }

        /// <summary>Releases nothing: the walk holds nothing to release.</summary>
        public void Dispose()
        {
        }
    }

    /// <summary>Walks SPI buses in order.</summary>
    internal sealed class SpiBusWalker : IEnumerator<SpiBusInfo>
    {
        private readonly SpiBusInfo[] _items;
        private int _index;

        internal SpiBusWalker(SpiBusInfo[] items)
        {
            _items = items;
            _index = -1;
        }

        /// <summary>The bus the walk is at.</summary>
        /// <exception cref="System.InvalidOperationException">The walk has not begun, or has
        /// ended.</exception>
        public SpiBusInfo Current
        {
            get
            {
                if (_index < 0 || _index >= _items.Length) throw new System.InvalidOperationException(FactsTable.NotWalking);
                return _items[_index];
            }
        }

        object IEnumerator.Current
        {
            get { return Current; }
        }

        /// <summary>Moves to the next bus, answering false past the last.</summary>
        public bool MoveNext()
        {
            if (_index < _items.Length) _index++;
            return _index < _items.Length;
        }

        /// <summary>Moves back to before the first bus.</summary>
        public void Reset()
        {
            _index = -1;
        }

        /// <summary>Releases nothing: the walk holds nothing to release.</summary>
        public void Dispose()
        {
        }
    }
}
