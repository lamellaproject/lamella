// A Lamella.Hardware.I2cDriver for the Nordic nRF51 (the polled two-wire master), over
using System;
using System.Device.I2c;
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Nrf51I2cDriver : I2cDriver
{
    private readonly uint _tasksStartRx;
    private readonly uint _tasksStartTx;
    private readonly uint _tasksStop;
    private readonly uint _tasksResume;
    private readonly uint _eventsStopped;
    private readonly uint _eventsRxdReady;
    private readonly uint _eventsTxdSent;
    private readonly uint _eventsError;
    private readonly uint _shorts;
    private readonly uint _intenclr;
    private readonly uint _errorsrc;
    private readonly uint _enable;
    private readonly uint _pselScl;
    private readonly uint _pselSda;
    private readonly uint _rxd;
    private readonly uint _txd;
    private readonly uint _frequency;
    private readonly uint _address;

    private readonly uint _pselSclValue;
    private readonly uint _pselSdaValue;
    private readonly uint _pinCnfSclReg;
    private readonly uint _pinCnfSdaReg;

    private readonly uint _bbSuspend;
    private readonly uint _bbStop;
    private readonly uint _errAnack;
    private readonly uint _errDnack;

    const int SpinCap = 20000;

    public Nrf51I2cDriver(Nrf51TwiBinding binding)
    {
        uint twi = binding.TwiBase;
        _tasksStartRx = twi + Nrf51TwiLayout.TASKS_STARTRX_OFF;
        _tasksStartTx = twi + Nrf51TwiLayout.TASKS_STARTTX_OFF;
        _tasksStop = twi + Nrf51TwiLayout.TASKS_STOP_OFF;
        _tasksResume = twi + Nrf51TwiLayout.TASKS_RESUME_OFF;
        _eventsStopped = twi + Nrf51TwiLayout.EVENTS_STOPPED_OFF;
        _eventsRxdReady = twi + Nrf51TwiLayout.EVENTS_RXDREADY_OFF;
        _eventsTxdSent = twi + Nrf51TwiLayout.EVENTS_TXDSENT_OFF;
        _eventsError = twi + Nrf51TwiLayout.EVENTS_ERROR_OFF;
        _shorts = twi + Nrf51TwiLayout.SHORTS_OFF;
        _intenclr = twi + Nrf51TwiLayout.INTENCLR_OFF;
        _errorsrc = twi + Nrf51TwiLayout.ERRORSRC_OFF;
        _enable = twi + Nrf51TwiLayout.ENABLE_OFF;
        _pselScl = twi + Nrf51TwiLayout.PSELSCL_OFF;
        _pselSda = twi + Nrf51TwiLayout.PSELSDA_OFF;
        _rxd = twi + Nrf51TwiLayout.RXD_OFF;
        _txd = twi + Nrf51TwiLayout.TXD_OFF;
        _frequency = twi + Nrf51TwiLayout.FREQUENCY_OFF;
        _address = twi + Nrf51TwiLayout.ADDRESS_OFF;

        _pselSclValue = binding.PselScl;
        _pselSdaValue = binding.PselSda;
        _pinCnfSclReg = binding.PinCnfSclReg;
        _pinCnfSdaReg = binding.PinCnfSdaReg;

        _bbSuspend = Nrf51TwiLayout.SHORTS_BB_SUSPEND;
        _bbStop = Nrf51TwiLayout.SHORTS_BB_STOP;
        _errAnack = Nrf51TwiLayout.ERRORSRC_ANACK;
        _errDnack = Nrf51TwiLayout.ERRORSRC_DNACK;
    }

    /// <summary>Brings the bound TWI up as a polled master: pins configured per the manual's
    /// TWI GPIO table (the layout's PIN_CNF_TWI value), routing latched while disabled, every
    /// relevant register written explicitly, ENABLE = TWI last. The chip's FREQUENCY register
    /// is enumerated, so <paramref name="busHz"/> must be exactly 100000, 250000 or 400000 --
    /// anything else throws.</summary>
    public override void Configure(int busHz)
    {
        uint frequency;
        if (busHz == 100000) frequency = Nrf51TwiLayout.FREQUENCY_K100;
        else if (busHz == 250000) frequency = Nrf51TwiLayout.FREQUENCY_K250;
        else if (busHz == 400000) frequency = Nrf51TwiLayout.FREQUENCY_K400;
        else
        {
#if LAMELLA_CORLIB_LINKED
            throw new ArgumentException("nrf51 twi rates are exactly 100000, 250000, or 400000");
#else
            throw new Exception("nrf51 twi rates are exactly 100000, 250000, or 400000");
#endif
        }

        Mmio.Write32(_enable, Nrf51TwiLayout.ENABLE_DISABLED);
        Mmio.Write32(_pinCnfSclReg, Nrf51TwiLayout.PIN_CNF_TWI);
        Mmio.Write32(_pinCnfSdaReg, Nrf51TwiLayout.PIN_CNF_TWI);
        Mmio.Write32(_pselScl, _pselSclValue);
        Mmio.Write32(_pselSda, _pselSdaValue);
        Mmio.Write32(_frequency, frequency);
        Mmio.Write32(_shorts, 0);
        Mmio.Write32(_intenclr, 0xFFFFFFFF);
        Mmio.Write32(_errorsrc, Nrf51TwiLayout.ERRORSRC_ALL_W1C);
        Mmio.Write32(_eventsStopped, 0);
        Mmio.Write32(_eventsRxdReady, 0);
        Mmio.Write32(_eventsTxdSent, 0);
        Mmio.Write32(_eventsError, 0);
        Mmio.Write32(_enable, Nrf51TwiLayout.ENABLE_TWI);
    }

    /// <summary>The block's write sequence: START, address+W, the bytes register-per-byte
    /// through TXD, STOP. Returns a status constant.</summary>
    public override int Write(int address, byte[] buffer, int count)
    {
        Mmio.Write32(_address, (uint)(address & 0x7F));
        Mmio.Write32(_eventsTxdSent, 0);
        Mmio.Write32(_eventsError, 0);
        Mmio.Write32(_eventsStopped, 0);
        Mmio.Write32(_shorts, 0);
        Mmio.Write32(_tasksStartTx, 1);
        for (int i = 0; i < count; i++)
        {
            Mmio.Write32(_txd, (uint)(buffer[i] & 0xFF));
            int rc = WaitTxdSent();
            if (rc != Ok) return rc;
        }
        Mmio.Write32(_tasksStop, 1);
        return WaitStopped();
    }

    /// <summary>The block's read sequence: START, address+R, the bytes extracted through the
    /// SHORTS-paced RXD protocol (BB_SUSPEND between bytes, BB_STOP pre-armed so the hardware
    /// NACK-STOPs the last). Returns a status constant.</summary>
    public override int Read(int address, byte[] buffer, int count)
    {
        if (count < 1) return OtherError;
        Mmio.Write32(_address, (uint)(address & 0x7F));
        Mmio.Write32(_eventsRxdReady, 0);
        Mmio.Write32(_eventsError, 0);
        Mmio.Write32(_eventsStopped, 0);
        Mmio.Write32(_shorts, count == 1 ? _bbStop : _bbSuspend);
        Mmio.Write32(_tasksStartRx, 1);
        return DrainReceive(buffer, count);
    }

    /// <summary>The block's write-then-read sequence: the write bytes go out with NO stop, then
    /// TASKS_STARTRX makes the hardware issue the REPEATED START for the read phase -- the
    /// register-read shape a sensor's sub-addressed read lives on.</summary>
    public override int WriteRead(int address, byte[] writeBuffer, int writeCount,
                                  byte[] readBuffer, int readCount)
    {
        if (readCount < 1) return OtherError;
        Mmio.Write32(_address, (uint)(address & 0x7F));
        Mmio.Write32(_eventsTxdSent, 0);
        Mmio.Write32(_eventsRxdReady, 0);
        Mmio.Write32(_eventsError, 0);
        Mmio.Write32(_eventsStopped, 0);
        Mmio.Write32(_shorts, 0);
        Mmio.Write32(_tasksStartTx, 1);
        for (int i = 0; i < writeCount; i++)
        {
            Mmio.Write32(_txd, (uint)(writeBuffer[i] & 0xFF));
            int rc = WaitTxdSent();
            if (rc != Ok) return rc;
        }
        Mmio.Write32(_shorts, readCount == 1 ? _bbStop : _bbSuspend);
        Mmio.Write32(_tasksStartRx, 1);
        return DrainReceive(readBuffer, readCount);
    }

    int DrainReceive(byte[] buffer, int count)
    {
        for (int index = 0; index < count; index++)
        {
            int rc = WaitRxdReady();
            if (rc != Ok) return rc;
            Mmio.Write32(_eventsRxdReady, 0);
            buffer[index] = (byte)(Mmio.Read32(_rxd) & 0xFFu);
            int remaining = count - 1 - index;
            if (remaining == 1) Mmio.Write32(_shorts, _bbStop);
            if (remaining > 0) Mmio.Write32(_tasksResume, 1);
        }
        return WaitStopped();
    }

    int WaitTxdSent()
    {
        for (int spin = 0; spin < SpinCap; spin++)
        {
            if (Mmio.Read32(_eventsError) != 0u) return RecoverError();
            if (Mmio.Read32(_eventsTxdSent) != 0u)
            {
                Mmio.Write32(_eventsTxdSent, 0);
                return Ok;
            }
        }
        return StopAfterFault();
    }

    int WaitRxdReady()
    {
        for (int spin = 0; spin < SpinCap; spin++)
        {
            if (Mmio.Read32(_eventsError) != 0u) return RecoverError();
            if (Mmio.Read32(_eventsRxdReady) != 0u) return Ok;
        }
        return StopAfterFault();
    }

    int WaitStopped()
    {
        for (int spin = 0; spin < SpinCap; spin++)
        {
            if (Mmio.Read32(_eventsStopped) != 0u)
            {
                Mmio.Write32(_eventsStopped, 0);
                return Ok;
            }
        }
        return OtherError;
    }

    int RecoverError()
    {
        Mmio.Write32(_eventsError, 0);
        uint source = Mmio.Read32(_errorsrc);
        Mmio.Write32(_errorsrc, Nrf51TwiLayout.ERRORSRC_ALL_W1C);
        Mmio.Write32(_tasksStop, 1);
        for (int spin = 0; spin < SpinCap; spin++)
        {
            if (Mmio.Read32(_eventsStopped) != 0u) break;
        }
        Mmio.Write32(_eventsStopped, 0);
        if ((source & _errAnack) != 0u) return AddressNack;
        if ((source & _errDnack) != 0u) return DataNack;
        return OtherError;
    }

    int StopAfterFault()
    {
        Mmio.Write32(_tasksStop, 1);
        for (int spin = 0; spin < SpinCap; spin++)
        {
            if (Mmio.Read32(_eventsStopped) != 0u) break;
        }
        Mmio.Write32(_eventsStopped, 0);
        return OtherError;
    }
}
