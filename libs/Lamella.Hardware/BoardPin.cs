// Lamella.Hardware.BoardPin -- one pin of the board's chip: its number, the labels the board prints for it, and what it can be.
using System.Collections.Generic;

namespace Lamella.Hardware
{
    /// <summary>One pin of the board's chip, as the board presents it.</summary>
    /// <param name="PinNumber">The number <c>GpioController.OpenPin</c> takes for the pin.</param>
    /// <param name="Labels">The names the board's facts state for the pin ("GP25" on a Raspberry Pi Pico
    /// 2), empty where they state none.</param>
    /// <param name="Functions">What the pin can be.</param>
    /// <param name="I2cBus">The I2C bus the pin's I2C function belongs to, or null where it has
    /// none.</param>
    /// <param name="SpiBus">The SPI bus the pin's SPI function belongs to, or null where it has
    /// none.</param>
    /// <param name="PwmChip">The chip number <c>TargetBoard.CreatePwmChannel</c> takes for the PWM
    /// channel that drives the pin, or null where no PWM channel drives it on this board.</param>
    /// <param name="PwmChannelNumber">The channel number that member takes for that channel, or null
    /// where no PWM channel drives the pin on this board.</param>
    /// <param name="AdcChannel">The channel of the chip's analog-to-digital converter that reads the
    /// pin, or null where none reads it on this board.</param>
    public sealed record BoardPin(int PinNumber, IReadOnlyList<string> Labels, PinFunctions Functions, int? I2cBus,
        int? SpiBus, int? PwmChip, int? PwmChannelNumber, int? AdcChannel)
    {
        /// <summary>Whether <paramref name="other"/> is a record of the same pin: every member equal.</summary>
        /// <param name="other">The record to compare, or null, which is not equal.</param>
        public bool Equals(BoardPin other)
        {
            return (object)other != null
                && PinNumber == other.PinNumber
                && object.Equals(Labels, other.Labels)
                && Functions == other.Functions
                && I2cBus == other.I2cBus
                && SpiBus == other.SpiBus
                && PwmChip == other.PwmChip
                && PwmChannelNumber == other.PwmChannelNumber
                && AdcChannel == other.AdcChannel;
        }

        /// <summary>A hash of the record's members, equal for records that are equal.</summary>
        public override int GetHashCode()
        {
            int hash = FactsTable.FoldHash(PinNumber, (object)Labels == null ? 0 : Labels.GetHashCode());
            hash = FactsTable.FoldHash(hash, (int)Functions);
            hash = FactsTable.FoldHash(hash, I2cBus.GetHashCode());
            hash = FactsTable.FoldHash(hash, SpiBus.GetHashCode());
            hash = FactsTable.FoldHash(hash, PwmChip.GetHashCode());
            hash = FactsTable.FoldHash(hash, PwmChannelNumber.GetHashCode());
            return FactsTable.FoldHash(hash, AdcChannel.GetHashCode());
        }
    }
}
