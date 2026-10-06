// A Lamella.Hardware.SpiDriver for the nRF52833's legacy SPI master: 8-bit frames, SPI modes 0 to
// 3, either bit order, and the rates its FREQUENCY register enumerates, 125 kHz to 8 MHz.
//
// The pins arrive routed from a board's generated binding. The master drives no chip select of its
// own, so the binding lists the bus's chip selects as GPIOs, numbered port * 32 + pin as the GPIO
// driver numbers them, and SpiConnectionSettings.ChipSelectLine is an index into that list, as
// dotnet/iot means it on Linux: line n is entry n, which this driver asserts across each operation,
// and -1 leaves selecting the device to the caller. SpiDevice refuses any other line, from the list
// this driver states, before it configures the driver.
using System.Device.Gpio;
using System.Device.Spi;
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Nrf52833SpiDriver : SpiDriver
{
    private readonly uint _eventsReady;
    private readonly uint _intenclr;
    private readonly uint _enable;
    private readonly uint _pselSck;
    private readonly uint _pselMosi;
    private readonly uint _pselMiso;
    private readonly uint _rxd;
    private readonly uint _txd;
    private readonly uint _frequency;
    private readonly uint _config;
    private readonly Nrf52833SpiBinding _binding;

    private int _chipSelectPin;
    private bool _chipSelectActiveHigh;
    private int _actualHz;

    const int Ok = 0;
    const int OtherError = 3;

    const int WaitBound = 100000;

    /// <summary>Binds the driver to one SPI wiring. No hardware is touched until
    /// <see cref="Configure"/>.</summary>
    public Nrf52833SpiDriver(Nrf52833SpiBinding binding)
    {
        _binding = binding;
        uint spi = binding.SpiBase;
        _eventsReady = spi + Nrf52833SpiLayout.EVENTS_READY_OFF;
        _intenclr = spi + Nrf52833SpiLayout.INTENCLR_OFF;
        _enable = spi + Nrf52833SpiLayout.ENABLE_OFF;
        _pselSck = spi + Nrf52833SpiLayout.PSEL_SCK_OFF;
        _pselMosi = spi + Nrf52833SpiLayout.PSEL_MOSI_OFF;
        _pselMiso = spi + Nrf52833SpiLayout.PSEL_MISO_OFF;
        _rxd = spi + Nrf52833SpiLayout.RXD_OFF;
        _txd = spi + Nrf52833SpiLayout.TXD_OFF;
        _frequency = spi + Nrf52833SpiLayout.FREQUENCY_OFF;
        _config = spi + Nrf52833SpiLayout.CONFIG_OFF;
        _chipSelectPin = -1;
    }

    /// <summary>Brings the master up for <paramref name="settings"/>, with the pins configured as
    /// the master requires and routed while it is disabled, and ENABLE written last.</summary>
    /// <remarks>The clock is the fastest rate the register enumerates at or below the request;
    /// <see cref="ActualClockFrequency"/> reports it.</remarks>
    /// <exception cref="System.ArgumentException">The frames are not 8 bits, the clock is below
    /// 125 kHz, or the binding names a chip select past this part's two GPIO ports.</exception>
    public override void Configure(SpiConnectionSettings settings)
    {
        if (settings.DataBitLength != 8)
        {
            Refuse("this SPI master's frames are 8 bits");
        }
        int line = settings.ChipSelectLine;
        int pin = -1;
        if (line >= 0)
        {
            // This part has two GPIO ports, so a pin is numbered 0 to 63 at most; whether it is
            // bonded out is the board's to know.
            pin = _binding.ChipSelectPins[line];
            if ((uint)pin >= 64u)
            {
                Refuse("the binding's chip select is not on either of this part's GPIO ports");
            }
        }
        uint frequency = FrequencyWord(settings.ClockFrequency);

        uint config = 0u;
        if (settings.DataFlow == DataFlow.LsbFirst)
        {
            config = config | Nrf52833SpiLayout.CONFIG_ORDER;
        }
        bool clockIdlesHigh = settings.Mode == SpiMode.Mode2 || settings.Mode == SpiMode.Mode3;
        if (settings.Mode == SpiMode.Mode1 || settings.Mode == SpiMode.Mode3)
        {
            config = config | Nrf52833SpiLayout.CONFIG_CPHA;
        }
        if (clockIdlesHigh)
        {
            config = config | Nrf52833SpiLayout.CONFIG_CPOL;
        }

        // Every serial personality at this base shares ENABLE, so writing it disabled is also the
        // step that disables them, and the pin routing below latches only while it reads disabled.
        Mmio.Write32(_enable, Nrf52833SpiLayout.ENABLE_DISABLED);

        // The pins before ENABLE: SCK an output idling at the clock polarity, with its input buffer
        // connected (the master needs it), MOSI an output at 0, MISO an input. Each level is
        // written before its pin becomes an output, so neither line glitches.
        DriveRoutedPin(_binding.PselSck, clockIdlesHigh);
        DriveRoutedPin(_binding.PselMosi, false);
        Mmio.Write32(_binding.PinCnfSckReg, Nrf52833SpiLayout.PIN_CNF_SPI_OUTPUT);
        Mmio.Write32(_binding.PinCnfMosiReg, Nrf52833SpiLayout.PIN_CNF_SPI_OUTPUT);
        Mmio.Write32(_binding.PinCnfMisoReg, Nrf52833SpiLayout.PIN_CNF_SPI_INPUT);
        Mmio.Write32(_pselSck, _binding.PselSck);
        Mmio.Write32(_pselMosi, _binding.PselMosi);
        Mmio.Write32(_pselMiso, _binding.PselMiso);

        // Disabling a sibling personality resets none of the registers it shares with this one,
        // so each register the transfer relies on is written rather than assumed.
        Mmio.Write32(_frequency, frequency);
        Mmio.Write32(_config, config);
        Mmio.Write32(_intenclr, Nrf52833SpiLayout.INTENCLR_READY);
        Mmio.Write32(_eventsReady, 0);

        _chipSelectPin = pin;
        _chipSelectActiveHigh = settings.ChipSelectLineActiveState == PinValue.High;
        if (pin >= 0)
        {
            // The select idles deasserted, and the level is written before the pin becomes an output.
            SetChipSelect(false);
            Mmio.Write32(PinCnfAddress(pin), Nrf52833SpiLayout.PIN_CNF_SPI_OUTPUT);
        }
        Mmio.Write32(_enable, Nrf52833SpiLayout.ENABLE_SPI);
    }

    /// <summary>Clocks <paramref name="count"/> bytes out while clocking as many in: an empty
    /// write buffer sends zeros, and an empty read buffer discards what arrives.</summary>
    /// <returns>0 when every byte completed; 3 when the master stopped raising its ready event,
    /// which leaves the rest of the transfer unsent.</returns>
    public override int TransferFullDuplex(System.ReadOnlySpan<byte> writeBuffer,
                                           System.Span<byte> readBuffer, int count)
    {
        for (int i = 0; i < count; i++)
        {
            uint tx = writeBuffer.IsEmpty ? 0u : (uint)writeBuffer[i];
            Mmio.Write32(_txd, tx);
            if (!WaitReady())
            {
                return OtherError;
            }
            // READY is cleared on every byte: it rises only after the byte has reached RXD.
            Mmio.Write32(_eventsReady, 0);
            uint rx = Mmio.Read32(_rxd) & Nrf52833SpiLayout.RXD_RXD;
            if (!readBuffer.IsEmpty)
            {
                readBuffer[i] = (byte)rx;
            }
        }
        return Ok;
    }

    /// <summary>Drives the chip select GPIO the settings' line selects; does nothing when the line
    /// is -1.</summary>
    public override void SetChipSelect(bool asserted)
    {
        if (_chipSelectPin < 0) return;
        bool high = asserted == _chipSelectActiveHigh;
        uint offset = high ? Nrf52833GpioLayout.OUTSET_OFF : Nrf52833GpioLayout.OUTCLR_OFF;
        Mmio.Write32(PortBase(_chipSelectPin >> 5) + offset, 1u << (_chipSelectPin & 31));
    }

    /// <summary>The number of chip selects the binding names for this bus.</summary>
    public override int ChipSelectCount { get { return _binding.ChipSelectPins.Length; } }

    /// <summary>The GPIO chip-select line <paramref name="line"/> drives: the binding's table entry,
    /// numbered port * 32 + pin.</summary>
    public override int GetChipSelectPin(int line) { return _binding.ChipSelectPins[line]; }

    /// <summary>The bit rate the master runs at: the fastest enumerated rate at or below the
    /// request.</summary>
    public override int ActualClockFrequency { get { return _actualHz; } }

    bool WaitReady()
    {
        for (int polls = 0; polls < WaitBound; polls = polls + 1)
        {
            if (Mmio.Read32(_eventsReady) != 0u) return true;
        }
        return false;
    }

    // The fastest rate the FREQUENCY register enumerates at or below the request.
    uint FrequencyWord(int hz)
    {
        if (hz >= 8000000) { _actualHz = 8000000; return Nrf52833SpiLayout.FREQUENCY_M8; }
        if (hz >= 4000000) { _actualHz = 4000000; return Nrf52833SpiLayout.FREQUENCY_M4; }
        if (hz >= 2000000) { _actualHz = 2000000; return Nrf52833SpiLayout.FREQUENCY_M2; }
        if (hz >= 1000000) { _actualHz = 1000000; return Nrf52833SpiLayout.FREQUENCY_M1; }
        if (hz >= 500000) { _actualHz = 500000; return Nrf52833SpiLayout.FREQUENCY_K500; }
        if (hz >= 250000) { _actualHz = 250000; return Nrf52833SpiLayout.FREQUENCY_K250; }
        if (hz >= 125000) { _actualHz = 125000; return Nrf52833SpiLayout.FREQUENCY_K125; }
        Refuse("the slowest rate this SPI master runs at is 125 kHz");
        return 0u;
    }

    // Sets a routed pin's output level. The three PSEL registers share one layout: the port in the
    // PORT field and the pin in the PIN field.
    static void DriveRoutedPin(uint psel, bool high)
    {
        uint port = (psel & Nrf52833SpiLayout.PSEL_SCK_PORT) >> (int)Nrf52833SpiLayout.PSEL_SCK_PORT_LSB;
        uint pin = (psel & Nrf52833SpiLayout.PSEL_SCK_PIN) >> (int)Nrf52833SpiLayout.PSEL_SCK_PIN_LSB;
        uint offset = high ? Nrf52833GpioLayout.OUTSET_OFF : Nrf52833GpioLayout.OUTCLR_OFF;
        Mmio.Write32(PortBase((int)port) + offset, 1u << (int)pin);
    }

    static uint PortBase(int port)
    {
        return port == 0 ? Nrf52833Instances.PORT0_BASE : Nrf52833Instances.PORT1_BASE;
    }

    static uint PinCnfAddress(int line)
    {
        return PortBase(line >> 5) + Nrf52833GpioLayout.PIN_CNF0_OFF
            + (uint)(line & 31) * Nrf52833GpioLayout.PIN_CNF_STRIDE;
    }

    static void Refuse(string why)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.ArgumentException(why);
#else
        throw new System.Exception(why);
#endif
    }
}
