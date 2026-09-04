// A Lamella.Hardware.I2cDriver layer-1 master for the Microchip SAM E54's SERCOM. Subclasses the
using System.Device.I2c;
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Same54I2cDriver : I2cDriver
{
    const int WaitBound = 100000;

    readonly Same54SercomI2cBinding _binding;
    readonly uint _ctrla;
    readonly uint _ctrlb;
    readonly uint _baud;
    readonly uint _intflag;
    readonly uint _status;
    readonly uint _syncbusy;
    readonly uint _addr;
    readonly uint _data;

    public Same54I2cDriver(Same54SercomI2cBinding binding)
    {
        _binding = binding;
        uint block = binding.SercomBase;
        _ctrla = block + Same54SercomI2cMasterLayout.CTRLA_OFF;
        _ctrlb = block + Same54SercomI2cMasterLayout.CTRLB_OFF;
        _baud = block + Same54SercomI2cMasterLayout.BAUD_OFF;
        _intflag = block + Same54SercomI2cMasterLayout.INTFLAG_OFF;
        _status = block + Same54SercomI2cMasterLayout.STATUS_OFF;
        _syncbusy = block + Same54SercomI2cMasterLayout.SYNCBUSY_OFF;
        _addr = block + Same54SercomI2cMasterLayout.ADDR_OFF;
        _data = block + Same54SercomI2cMasterLayout.DATA_OFF;
    }

    public override void Configure(int busHz)
    {
        Mmio.Write32(_binding.ApbMaskReg, Mmio.Read32(_binding.ApbMaskReg) | _binding.ApbMask);
        Mmio.Write32(_binding.GclkPchctrlReg, _binding.GclkPchctrlValue);

        Mmio.Write8(_binding.PmuxReg, (byte)_binding.PmuxPair);
        byte pinConfig = (byte)(Same54PortLayout.PINCFG0_PMUXEN | Same54PortLayout.PINCFG0_INEN);
        Mmio.Write8(_binding.PincfgSdaReg, pinConfig);
        Mmio.Write8(_binding.PincfgSclReg, pinConfig);

        Mmio.Write32(_ctrla, Same54SercomI2cMasterLayout.CTRLA_SWRST);
        WaitSync(Same54SercomI2cMasterLayout.SYNCBUSY_SWRST);

        Mmio.Write32(_ctrla, Same54SercomI2cMasterLayout.MODE_I2C_MASTER
            << (int)Same54SercomI2cMasterLayout.CTRLA_MODE_LSB);
        Mmio.Write32(_ctrlb, 0);

        uint divisor = 0;
        if (busHz > 0)
        {
            uint half = _binding.CoreClockHz / (uint)(busHz * 2);
            divisor = half > 5 ? half - 5 : 0;
            if (divisor > Same54SercomI2cMasterLayout.BAUD_BAUD)
            {
                divisor = Same54SercomI2cMasterLayout.BAUD_BAUD;
            }
        }
        Mmio.Write32(_baud, divisor & Same54SercomI2cMasterLayout.BAUD_BAUD);

        Mmio.Write32(_ctrla, Mmio.Read32(_ctrla) | Same54SercomI2cMasterLayout.CTRLA_ENABLE);
        WaitSync(Same54SercomI2cMasterLayout.SYNCBUSY_ENABLE);

        ForceIdle();
    }

    public override int Write(int address, System.ReadOnlySpan<byte> buffer, int count)
    {
        int started = Start(address, Same54SercomI2cMasterLayout.DIRECTION_WRITE);
        if (started != Ok)
        {
            return started;
        }
        for (int index = 0; index < count; index++)
        {
            Mmio.Write8(_data, buffer[index]);
            if (!WaitFlag(Same54SercomI2cMasterLayout.INTFLAG_MB))
            {
                Stop();
                return OtherError;
            }
            if (Nacked())
            {
                Stop();
                return DataNack;
            }
        }
        Stop();
        return Ok;
    }

    public override int Read(int address, System.Span<byte> buffer, int count)
    {
        int started = Start(address, Same54SercomI2cMasterLayout.DIRECTION_READ);
        if (started != Ok)
        {
            return started;
        }
        return ReadPhase(buffer, count);
    }

    public override int WriteRead(int address, System.ReadOnlySpan<byte> writeBuffer, int writeCount,
        System.Span<byte> readBuffer, int readCount)
    {
        int started = Start(address, Same54SercomI2cMasterLayout.DIRECTION_WRITE);
        if (started != Ok)
        {
            return started;
        }
        for (int index = 0; index < writeCount; index++)
        {
            Mmio.Write8(_data, writeBuffer[index]);
            if (!WaitFlag(Same54SercomI2cMasterLayout.INTFLAG_MB))
            {
                Stop();
                return OtherError;
            }
            if (Nacked())
            {
                Stop();
                return DataNack;
            }
        }
        int restarted = Start(address, Same54SercomI2cMasterLayout.DIRECTION_READ);
        if (restarted != Ok)
        {
            return restarted;
        }
        return ReadPhase(readBuffer, readCount);
    }

    /// <summary>Issues a start (or repeated start) and the addressed direction byte, returning a
    /// layer-1 status. An unanswered address comes back as AddressNack, not an error.</summary>
    int Start(int address, uint direction)
    {
        EnsureIdle();
        ClearFlags();

        uint addressed = (((uint)address << 1) | direction) & Same54SercomI2cMasterLayout.ADDR_ADDR;
        Mmio.Write32(_addr, addressed);
        if (!WaitFlag(Same54SercomI2cMasterLayout.INTFLAG_MB | Same54SercomI2cMasterLayout.INTFLAG_SB))
        {
            Stop();
            return OtherError;
        }
        if (Errored())
        {
            ClearErrors();
            Stop();
            return OtherError;
        }
        if (Nacked())
        {
            Stop();
            return AddressNack;
        }
        return Ok;
    }

    /// <summary>Brings the bus to a state a transaction can legitimately start from.
    ///
    /// Only UNKNOWN may be forced -- the datasheet permits software to write IDLE and no other
    /// state -- and OWNER must be left alone, because that is what a repeated start rides on.</summary>
    void EnsureIdle()
    {
        uint state = (Mmio.Read16(_status) & Same54SercomI2cMasterLayout.STATUS_BUSSTATE)
            >> (int)Same54SercomI2cMasterLayout.STATUS_BUSSTATE_LSB;
        if (state == Same54SercomI2cMasterLayout.BUSSTATE_UNKNOWN)
        {
            ForceIdle();
        }
    }

    void ClearFlags()
    {
        Mmio.Write8(_intflag, (byte)(Same54SercomI2cMasterLayout.INTFLAG_MB
            | Same54SercomI2cMasterLayout.INTFLAG_SB | Same54SercomI2cMasterLayout.INTFLAG_ERROR));
    }

    /// <summary>Whether the bus itself failed, as opposed to a slave declining to answer.</summary>
    bool Errored()
    {
        return (Mmio.Read16(_status) & (Same54SercomI2cMasterLayout.STATUS_BUSERR
            | Same54SercomI2cMasterLayout.STATUS_ARBLOST)) != 0;
    }

    void ClearErrors()
    {
        Mmio.Write16(_status, (ushort)(Same54SercomI2cMasterLayout.STATUS_BUSERR
            | Same54SercomI2cMasterLayout.STATUS_ARBLOST));
        ClearFlags();
    }

    /// <summary>Receives `count` bytes and closes the transaction. The first byte is already in
    /// hand when this is entered -- addressing a slave for a read clocks it in.</summary>
    /// <remarks>DO NOT ADD LOGGING, TRACING OR ANY OTHER SLOW WORK INSIDE THIS LOOP. The master
    /// holds SCL low for as long as a flag is pending, so a pause here is not a delay -- it is a
    /// held bus, and a device with an SMBus timeout abandons the transfer and releases it. Every
    /// byte after that reads as 0xFF from a bus nothing is driving, which looks exactly like a
    /// sequencing defect in this method. A probe with two console writes between the bytes
    /// reproduced that perfectly against a device whose limit is 25 ms. Gather first, print
    /// after.</remarks>
    int ReadPhase(System.Span<byte> buffer, int count)
    {
        if (count <= 0)
        {
            Stop();
            return Ok;
        }
        for (int index = 0; index < count; index++)
        {
            if (index > 0 && !WaitFlag(Same54SercomI2cMasterLayout.INTFLAG_SB))
            {
                Stop();
                return OtherError;
            }
            buffer[index] = Mmio.Read8(_data);
            bool last = index == count - 1;
            SetAckAction(last ? Same54SercomI2cMasterLayout.ACKACT_NACK
                : Same54SercomI2cMasterLayout.ACKACT_ACK);
            Command(last ? Same54SercomI2cMasterLayout.CMD_STOP
                : Same54SercomI2cMasterLayout.CMD_BYTE_READ);
        }
        return Ok;
    }

    void SetAckAction(uint action)
    {
        uint control = Mmio.Read32(_ctrlb) & ~Same54SercomI2cMasterLayout.CTRLB_ACKACT;
        Mmio.Write32(_ctrlb, control
            | (action << (int)Same54SercomI2cMasterLayout.CTRLB_ACKACT_LSB));
    }

    void Command(uint command)
    {
        uint control = Mmio.Read32(_ctrlb) & ~Same54SercomI2cMasterLayout.CTRLB_CMD;
        Mmio.Write32(_ctrlb, control
            | (command << (int)Same54SercomI2cMasterLayout.CTRLB_CMD_LSB));
        WaitSync(Same54SercomI2cMasterLayout.SYNCBUSY_SYSOP);
    }

    void Stop()
    {
        SetAckAction(Same54SercomI2cMasterLayout.ACKACT_NACK);
        Command(Same54SercomI2cMasterLayout.CMD_STOP);
    }

    void ForceIdle()
    {
        uint state = Mmio.Read16(_status) & ~Same54SercomI2cMasterLayout.STATUS_BUSSTATE;
        Mmio.Write16(_status, (ushort)(state | (Same54SercomI2cMasterLayout.BUSSTATE_IDLE
            << (int)Same54SercomI2cMasterLayout.STATUS_BUSSTATE_LSB)));
        WaitSync(Same54SercomI2cMasterLayout.SYNCBUSY_SYSOP);
    }

    /// <summary>Whether the last address or data byte went unacknowledged.</summary>
    bool Nacked()
    {
        return (Mmio.Read16(_status) & Same54SercomI2cMasterLayout.STATUS_RXNACK) != 0;
    }

    bool WaitFlag(uint flags)
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read8(_intflag) & flags) != 0)
            {
                return true;
            }
        }
        return false;
    }

    void WaitSync(uint bits)
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read32(_syncbusy) & bits) == 0)
            {
                return;
            }
        }
    }
}
