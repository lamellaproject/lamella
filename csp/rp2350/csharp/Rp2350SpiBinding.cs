// The descriptor an RP2350 PL022-SPI driver consumes: one spi
namespace Lamella.Boards
{
    public sealed class Rp2350SpiBinding
    {
        /// <summary>The bound PL022 instance's base address.</summary>
        public readonly uint SspBase;
        /// <summary>The RESETS release set (the spi instance plus both IO banks).</summary>
        public readonly uint ResetMask;
        /// <summary>IO_BANK0 CTRL / PADS_BANK0 addresses of the MISO pin (the block's RX).</summary>
        public readonly uint IoMisoCtrl;
        public readonly uint PadsMiso;
        /// <summary>IO_BANK0 CTRL / PADS_BANK0 addresses of the SCK pin.</summary>
        public readonly uint IoSckCtrl;
        public readonly uint PadsSck;
        /// <summary>IO_BANK0 CTRL / PADS_BANK0 addresses of the MOSI pin (the block's TX).</summary>
        public readonly uint IoMosiCtrl;
        public readonly uint PadsMosi;
        /// <summary>The function-select value routing the pins to the bound instance.</summary>
        public readonly uint Funcsel;
        /// <summary>SSPCLK under the board's default plan (clk_peri, crystal-exact).</summary>
        public readonly uint SspclkHz;
        /// <summary>The bus's chip selects, as GPIO numbers in line order: entry n is the pin
        /// <c>SpiConnectionSettings.ChipSelectLine</c> n selects, and entry 0 is the chip select the
        /// board names for this bus. Empty when the board names none.</summary>
        public readonly int[] ChipSelectPins;

        /// <param name="chipSelectPins">The bus's chip selects in line order; never null, and empty
        /// when the board names none.</param>
        public Rp2350SpiBinding(uint sspBase, uint resetMask,
            uint ioMisoCtrl, uint padsMiso, uint ioSckCtrl, uint padsSck,
            uint ioMosiCtrl, uint padsMosi, uint funcsel, uint sspclkHz, int[] chipSelectPins)
        {
            SspBase = sspBase;
            ResetMask = resetMask;
            IoMisoCtrl = ioMisoCtrl;
            PadsMiso = padsMiso;
            IoSckCtrl = ioSckCtrl;
            PadsSck = padsSck;
            IoMosiCtrl = ioMosiCtrl;
            PadsMosi = padsMosi;
            Funcsel = funcsel;
            SspclkHz = sspclkHz;
            ChipSelectPins = chipSelectPins;
        }
    }
}
