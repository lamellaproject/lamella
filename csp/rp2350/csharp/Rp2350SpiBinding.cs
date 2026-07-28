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
        /// <summary>IO_BANK0 CTRL / PADS_BANK0 addresses of the hardware chip-select pin
        /// (the PL022's ss_n, which frames every 8-bit transfer when routed).</summary>
        public readonly uint IoCsCtrl;
        public readonly uint PadsCs;
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

        public Rp2350SpiBinding(uint sspBase, uint resetMask,
            uint ioMisoCtrl, uint padsMiso, uint ioCsCtrl, uint padsCs,
            uint ioSckCtrl, uint padsSck, uint ioMosiCtrl, uint padsMosi,
            uint funcsel, uint sspclkHz)
        {
            SspBase = sspBase;
            ResetMask = resetMask;
            IoMisoCtrl = ioMisoCtrl;
            PadsMiso = padsMiso;
            IoCsCtrl = ioCsCtrl;
            PadsCs = padsCs;
            IoSckCtrl = ioSckCtrl;
            PadsSck = padsSck;
            IoMosiCtrl = ioMosiCtrl;
            PadsMosi = padsMosi;
            Funcsel = funcsel;
            SspclkHz = sspclkHz;
        }
    }
}
