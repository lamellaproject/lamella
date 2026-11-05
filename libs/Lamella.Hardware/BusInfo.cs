// Lamella.Hardware.I2cBusInfo and SpiBusInfo -- the buses the board serves.
using System.Collections.Generic;

namespace Lamella.Hardware
{
    /// <summary>One I2C bus the board serves.</summary>
    /// <param name="Number">The bus's number, the one <c>I2cConnectionSettings.BusId</c> takes.</param>
    /// <param name="Sda">The bus's data pin.</param>
    /// <param name="Scl">The bus's clock pin.</param>
    /// <param name="Frequencies">The clock frequencies in hertz the bus serves.</param>
    public sealed record I2cBusInfo(int Number, BoardPin Sda, BoardPin Scl, IReadOnlyList<int> Frequencies)
    {
        /// <summary>Whether <paramref name="other"/> is a record of the same bus: every member equal.</summary>
        /// <param name="other">The record to compare, or null, which is not equal.</param>
        public bool Equals(I2cBusInfo other)
        {
            return (object)other != null
                && Number == other.Number
                && object.Equals(Sda, other.Sda)
                && object.Equals(Scl, other.Scl)
                && object.Equals(Frequencies, other.Frequencies);
        }

        /// <summary>A hash of the record's members, equal for records that are equal.</summary>
        public override int GetHashCode()
        {
            int hash = FactsTable.FoldHash(Number, (object)Sda == null ? 0 : Sda.GetHashCode());
            hash = FactsTable.FoldHash(hash, (object)Scl == null ? 0 : Scl.GetHashCode());
            return FactsTable.FoldHash(hash, (object)Frequencies == null ? 0 : Frequencies.GetHashCode());
        }
    }

    /// <summary>One SPI bus the board serves.</summary>
    /// <param name="Number">The bus's number, the one <c>SpiConnectionSettings.BusId</c> takes.</param>
    /// <param name="Sck">The bus's clock pin.</param>
    /// <param name="Mosi">The pin on which the bus's controller sends.</param>
    /// <param name="Miso">The pin on which the bus's controller receives.</param>
    /// <param name="ChipSelects">The bus's chip-select pins, by the line
    /// <c>SpiConnectionSettings.ChipSelectLine</c> names.</param>
    /// <param name="MinFrequency">The slowest clock frequency in hertz the bus makes.</param>
    /// <param name="MaxFrequency">The fastest clock frequency in hertz the bus makes.</param>
    public sealed record SpiBusInfo(int Number, BoardPin Sck, BoardPin Mosi, BoardPin Miso,
        IReadOnlyList<BoardPin> ChipSelects, int MinFrequency, int MaxFrequency)
    {
        /// <summary>Whether <paramref name="other"/> is a record of the same bus: every member equal.</summary>
        /// <param name="other">The record to compare, or null, which is not equal.</param>
        public bool Equals(SpiBusInfo other)
        {
            return (object)other != null
                && Number == other.Number
                && object.Equals(Sck, other.Sck)
                && object.Equals(Mosi, other.Mosi)
                && object.Equals(Miso, other.Miso)
                && object.Equals(ChipSelects, other.ChipSelects)
                && MinFrequency == other.MinFrequency
                && MaxFrequency == other.MaxFrequency;
        }

        /// <summary>A hash of the record's members, equal for records that are equal.</summary>
        public override int GetHashCode()
        {
            int hash = FactsTable.FoldHash(Number, (object)Sck == null ? 0 : Sck.GetHashCode());
            hash = FactsTable.FoldHash(hash, (object)Mosi == null ? 0 : Mosi.GetHashCode());
            hash = FactsTable.FoldHash(hash, (object)Miso == null ? 0 : Miso.GetHashCode());
            hash = FactsTable.FoldHash(hash, (object)ChipSelects == null ? 0 : ChipSelects.GetHashCode());
            hash = FactsTable.FoldHash(hash, MinFrequency);
            return FactsTable.FoldHash(hash, MaxFrequency);
        }
    }
}
