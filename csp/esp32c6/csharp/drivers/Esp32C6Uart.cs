// The ESP32-C6 HP-UART driver, in C# over Lamella.Hardware.Mmio -- ONE driver for every HP
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Esp32C6Uart
{
    private readonly uint _fifo;
    private readonly uint _clkdiv;
    private readonly uint _status;
    private readonly uint _conf0;
    private readonly uint _regUpdate;
    private readonly Esp32C6UartBinding _binding;

    /// <summary>Binds the driver to one HP-UART wiring; no hardware is touched until
    /// <see cref="Init"/>.</summary>
    public Esp32C6Uart(Esp32C6UartBinding binding)
    {
        _binding = binding;
        _fifo = binding.UartBase + Esp32c6UartLayout.FIFO_OFF;
        _clkdiv = binding.UartBase + Esp32c6UartLayout.CLKDIV_OFF;
        _status = binding.UartBase + Esp32c6UartLayout.STATUS_OFF;
        _conf0 = binding.UartBase + Esp32c6UartLayout.CONF0_OFF;
        _regUpdate = binding.UartBase + Esp32c6UartLayout.REG_UPDATE_OFF;
    }

    /// <summary>Brings the bound UART up at <paramref name="baud"/>, 8N1, from the plan's
    /// function clock: bus clock on + reset pulse, function clock selected (crystal, divider
    /// 1), the pins' IO_MUX functions routed (RX input buffer on), the FIFO reset pulsed,
    /// then the divisor -- committed through the REG_UPDATE handshake.</summary>
    public void Init(int baud)
    {
        Mmio.Write32(_binding.PcrConf,
            Esp32c6PcrLayout.UART0_CONF_CLK_EN | Esp32c6PcrLayout.UART0_CONF_RST_EN);
        Mmio.Write32(_binding.PcrConf, Esp32c6PcrLayout.UART0_CONF_CLK_EN);
        Mmio.Write32(_binding.PcrSclkConf, Esp32c6PcrLayout.UART0_SCLK_CONF_SCLK_EN
            | (Esp32c6PcrLayout.SCLK_SEL_XTAL << (int)Esp32c6PcrLayout.UART0_SCLK_CONF_SCLK_SEL_LSB));

        uint txWord = (Esp32c6IoMuxLayout.FUN_DRV_DEFAULT << (int)Esp32c6IoMuxLayout.GPIO0_FUN_DRV_LSB)
            | (_binding.McuSel << (int)Esp32c6IoMuxLayout.GPIO0_MCU_SEL_LSB);
        Mmio.Write32(_binding.IoMuxTx, txWord);
        Mmio.Write32(_binding.IoMuxRx, txWord | Esp32c6IoMuxLayout.GPIO0_FUN_IE);

        uint line8n1 = (Esp32c6UartLayout.BIT_NUM_8BIT << (int)Esp32c6UartLayout.CONF0_BIT_NUM_LSB)
            | (Esp32c6UartLayout.STOP_BITS_ONE << (int)Esp32c6UartLayout.CONF0_STOP_BIT_NUM_LSB)
            | Esp32c6UartLayout.CONF0_MEM_CLK_EN;
        Mmio.Write32(_conf0, line8n1
            | Esp32c6UartLayout.CONF0_RXFIFO_RST | Esp32c6UartLayout.CONF0_TXFIFO_RST);
        Mmio.Write32(_conf0, line8n1);

        uint sixteenths = (_binding.SclkHz * 16u) / (uint)baud;
        Mmio.Write32(_clkdiv, (sixteenths >> 4)
            | ((sixteenths & 0xFu) << (int)Esp32c6UartLayout.CLKDIV_CLKDIV_FRAG_LSB));

        CommitConfig();
    }

    /// <summary>Ties TX to RX inside the peripheral (the wired pins keep working) -- the
    /// no-hands round-trip self-test.</summary>
    public void EnableLoopback(bool enabled)
    {
        uint line8n1 = (Esp32c6UartLayout.BIT_NUM_8BIT << (int)Esp32c6UartLayout.CONF0_BIT_NUM_LSB)
            | (Esp32c6UartLayout.STOP_BITS_ONE << (int)Esp32c6UartLayout.CONF0_STOP_BIT_NUM_LSB)
            | Esp32c6UartLayout.CONF0_MEM_CLK_EN;
        Mmio.Write32(_conf0, enabled ? (line8n1 | Esp32c6UartLayout.CONF0_LOOPBACK) : line8n1);
        CommitConfig();
    }

    /// <summary>The REG_UPDATE handshake: config crosses into the UART core's clock domain
    /// when the hardware clears the bit. Bounded so a host register simulator with no UART
    /// model cannot hang the driver.</summary>
    void CommitConfig()
    {
        Mmio.Write32(_regUpdate, Esp32c6UartLayout.REG_UPDATE_REG_UPDATE);
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_regUpdate) & Esp32c6UartLayout.REG_UPDATE_REG_UPDATE) == 0u) return;
        }
    }

    /// <summary>Sends one byte, waiting for FIFO room first.</summary>
    public void WriteByte(int value)
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            uint txCount = (Mmio.Read32(_status) & Esp32c6UartLayout.STATUS_TXFIFO_CNT)
                >> (int)Esp32c6UartLayout.STATUS_TXFIFO_CNT_LSB;
            if (txCount < Esp32c6UartLayout.FIFO_DEPTH) break;
        }
        Mmio.Write32(_fifo, (uint)(value & 0xFF));
    }

    /// <summary>Sends a string as its low-byte (ASCII) characters.</summary>
    public void Write(string text)
    {
        for (int i = 0; i < text.Length; i++)
        {
            WriteByte(text[i]);
        }
    }

    /// <summary>Waits until the TX FIFO is fully drained (mode switches must not cut a frame).</summary>
    public void Flush()
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_status) & Esp32c6UartLayout.STATUS_TXFIFO_CNT) == 0u) return;
        }
    }

    /// <summary>How many received bytes wait in the RX FIFO.</summary>
    public int Available
    {
        get { return (int)(Mmio.Read32(_status) & Esp32c6UartLayout.STATUS_RXFIFO_CNT); }
    }

    /// <summary>Pops one received byte, or -1 when the RX FIFO is empty (the Stream convention).</summary>
    public int ReadByte()
    {
        if (Available == 0) return -1;
        return (int)(Mmio.Read32(_fifo) & Esp32c6UartLayout.FIFO_RXFIFO_RD_BYTE);
    }
}
