// The descriptor an RP2350 DW-I2C driver consumes: one i2c
namespace Lamella.Boards
{
    public sealed class Rp2350I2cBinding
    {
        /// <summary>The bound DW_apb_i2c instance's base address.</summary>
        public readonly uint I2cBase;
        /// <summary>The RESETS release set (the i2c instance plus both IO banks).</summary>
        public readonly uint ResetMask;
        /// <summary>IO_BANK0 CTRL / PADS_BANK0 addresses of the SDA pin.</summary>
        public readonly uint IoSdaCtrl;
        public readonly uint PadsSda;
        /// <summary>IO_BANK0 CTRL / PADS_BANK0 addresses of the SCL pin.</summary>
        public readonly uint IoSclCtrl;
        public readonly uint PadsScl;
        /// <summary>The function-select value routing both pins to the bound instance.</summary>
        public readonly uint Funcsel;
        /// <summary>ic_clk under the board's default plan (clk_sys; the pico-sdk count
        /// formulas divide it at Configure).</summary>
        public readonly uint IcClkHz;

        public Rp2350I2cBinding(uint i2cBase, uint resetMask,
            uint ioSdaCtrl, uint padsSda, uint ioSclCtrl, uint padsScl,
            uint funcsel, uint icClkHz)
        {
            I2cBase = i2cBase;
            ResetMask = resetMask;
            IoSdaCtrl = ioSdaCtrl;
            PadsSda = padsSda;
            IoSclCtrl = ioSclCtrl;
            PadsScl = padsScl;
            Funcsel = funcsel;
            IcClkHz = icClkHz;
        }
    }
}
