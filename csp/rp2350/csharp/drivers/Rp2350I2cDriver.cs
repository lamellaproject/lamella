// A Lamella.Hardware.I2cDriver for the RP2350's Synopsys DW_apb_i2c, over
using System;
using System.Device.I2c;
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Rp2350I2cDriver : I2cDriver
{
    private readonly uint _con;
    private readonly uint _tar;
    private readonly uint _dataCmd;
    private readonly uint _hcnt;
    private readonly uint _lcnt;
    private readonly uint _rawIntrStat;
    private readonly uint _rxTl;
    private readonly uint _txTl;
    private readonly uint _clrTxAbrt;
    private readonly uint _clrStopDet;
    private readonly uint _enable;
    private readonly uint _enableStatus;
    private readonly uint _status;
    private readonly uint _rxflr;
    private readonly uint _sdaHold;
    private readonly uint _txAbrtSource;
    private readonly uint _spklen;
    private readonly uint _resetsSet;
    private readonly uint _resetsClr;
    private readonly uint _resetsDone;
    private readonly uint _sdaStatus;
    private readonly uint _sclStatus;
    private readonly uint _timerRaw;
    private readonly uint _cmdRead;
    private readonly uint _cmdStop;
    private readonly uint _cmdRestart;
    private readonly uint _intTxEmpty;
    private readonly uint _intTxAbrt;
    private readonly uint _intStopDet;
    private readonly uint _statusTfnf;
    private readonly uint _abrtAddrNoack;
    private readonly uint _abrtDataNoack;
    private readonly uint _enableOn;
    private readonly uint _enabledNow;
    private readonly uint _padHigh;
    private readonly uint _blockReset;
    private readonly uint _pinLow;
    private readonly uint _pinReleased;
    private readonly Rp2350I2cBinding _binding;
    private int _busHz;
    private uint _waitUs;

    const uint ClockLowLimitUs = 25000;
    const uint QueuedBitTimes = 180;
    const uint ClearHalfPeriodUs = 6;
    const uint ClearBudgetUs = 5000;
    const int ClockPolls = 10000;
    const int ReadAborted = -1;
    const int ReadStalled = -2;

    /// <summary>Binds the driver to one DW wiring; no hardware is touched until
    /// <see cref="Configure"/>.</summary>
    public Rp2350I2cDriver(Rp2350I2cBinding binding)
    {
        _binding = binding;
        uint i2c = binding.I2cBase;
        _con = i2c + Rp2350I2cLayout.IC_CON_OFF;
        _tar = i2c + Rp2350I2cLayout.IC_TAR_OFF;
        _dataCmd = i2c + Rp2350I2cLayout.IC_DATA_CMD_OFF;
        _hcnt = i2c + Rp2350I2cLayout.IC_FS_SCL_HCNT_OFF;
        _lcnt = i2c + Rp2350I2cLayout.IC_FS_SCL_LCNT_OFF;
        _rawIntrStat = i2c + Rp2350I2cLayout.IC_RAW_INTR_STAT_OFF;
        _rxTl = i2c + Rp2350I2cLayout.IC_RX_TL_OFF;
        _txTl = i2c + Rp2350I2cLayout.IC_TX_TL_OFF;
        _clrTxAbrt = i2c + Rp2350I2cLayout.IC_CLR_TX_ABRT_OFF;
        _clrStopDet = i2c + Rp2350I2cLayout.IC_CLR_STOP_DET_OFF;
        _enable = i2c + Rp2350I2cLayout.IC_ENABLE_OFF;
        _enableStatus = i2c + Rp2350I2cLayout.IC_ENABLE_STATUS_OFF;
        _status = i2c + Rp2350I2cLayout.IC_STATUS_OFF;
        _rxflr = i2c + Rp2350I2cLayout.IC_RXFLR_OFF;
        _sdaHold = i2c + Rp2350I2cLayout.IC_SDA_HOLD_OFF;
        _txAbrtSource = i2c + Rp2350I2cLayout.IC_TX_ABRT_SOURCE_OFF;
        _spklen = i2c + Rp2350I2cLayout.IC_FS_SPKLEN_OFF;
        _resetsSet = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_SET_OFF;
        _resetsClr = Rp2350Instances.RESETS_CLR_BASE + Rp2350ResetsLayout.RESET_OFF;
        _resetsDone = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_DONE_OFF;
        uint statusBelowCtrl = Rp2350IoBank0Layout.GPIO0_CTRL_OFF - Rp2350IoBank0Layout.GPIO0_STATUS_OFF;
        _sdaStatus = binding.IoSdaCtrl - statusBelowCtrl;
        _sclStatus = binding.IoSclCtrl - statusBelowCtrl;
        _timerRaw = Rp2350Instances.TIMER0_BASE + Rp2350TimerLayout.TIMERAWL_OFF;
        _cmdRead = Rp2350I2cLayout.IC_DATA_CMD_CMD;
        _cmdStop = Rp2350I2cLayout.IC_DATA_CMD_STOP;
        _cmdRestart = Rp2350I2cLayout.IC_DATA_CMD_RESTART;
        _intTxEmpty = Rp2350I2cLayout.IC_RAW_INTR_STAT_TX_EMPTY;
        _intTxAbrt = Rp2350I2cLayout.IC_RAW_INTR_STAT_TX_ABRT;
        _intStopDet = Rp2350I2cLayout.IC_RAW_INTR_STAT_STOP_DET;
        _statusTfnf = Rp2350I2cLayout.IC_STATUS_TFNF;
        _abrtAddrNoack = Rp2350I2cLayout.IC_TX_ABRT_SOURCE_ABRT_7B_ADDR_NOACK;
        _abrtDataNoack = Rp2350I2cLayout.IC_TX_ABRT_SOURCE_ABRT_TXDATA_NOACK;
        _enableOn = Rp2350I2cLayout.IC_ENABLE_ENABLE;
        _enabledNow = Rp2350I2cLayout.IC_ENABLE_STATUS_IC_EN;
        _padHigh = Rp2350IoBank0Layout.GPIO0_STATUS_INFROMPAD;
        _blockReset = binding.ResetMask
            & ~(Rp2350Instances.IO_BANK0_RESET_MASK | Rp2350Instances.PADS_BANK0_RESET_MASK);
        uint outLow = Rp2350IoBank0Layout.OUTOVER_LOW << (int)Rp2350IoBank0Layout.GPIO0_CTRL_OUTOVER_LSB;
        _pinLow = binding.Funcsel | outLow
            | (Rp2350IoBank0Layout.OEOVER_ENABLE << (int)Rp2350IoBank0Layout.GPIO0_CTRL_OEOVER_LSB);
        _pinReleased = binding.Funcsel | outLow
            | (Rp2350IoBank0Layout.OEOVER_DISABLE << (int)Rp2350IoBank0Layout.GPIO0_CTRL_OEOVER_LSB);
    }

    /// <summary>Brings the bound DW up as a 7-bit master at <paramref name="busHz"/>: 100000
    /// (standard mode), 400000 (fast mode) or 1000000 (fast mode plus). The block is reset and
    /// its pads prepared (de-isolated, input buffered, internally pulled up, schmitt kept for the
    /// open-drain edges), a data line an earlier program left held low is cleared, and the
    /// official pico-sdk counts are programmed against the binding's ic_clk, enable last. The
    /// faster modes need pull-ups stronger than the pads' own.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="busHz"/> is not one
    /// of the three rates.</exception>
    /// <exception cref="System.InvalidOperationException">TIMER0, the microsecond count every
    /// wait on the bus is timed against, is not counting.</exception>
    /// <exception cref="System.IO.IOException">The block did not come out of reset.</exception>
    public override void Configure(int busHz)
    {
        if (busHz != 100000 && busHz != 400000 && busHz != 1000000)
        {
            throw new ArgumentOutOfRangeException("busHz",
                "the RP2350's I2C runs at 100000, 400000 or 1000000 Hz");
        }
        RequireClock();
        _busHz = busHz;
        _waitUs = ClockLowLimitUs + (QueuedBitTimes * 1000000u + (uint)busHz - 1u) / (uint)busHz;
        if (!CycleBlock()) throw new System.IO.IOException("the RP2350's I2C block did not come out of reset");

        uint padI2c = Rp2350PadsBank0Layout.GPIO0_IE | Rp2350PadsBank0Layout.GPIO0_PUE
            | Rp2350PadsBank0Layout.GPIO0_SCHMITT;
        Mmio.Write32(_binding.PadsSda, padI2c);
        Mmio.Write32(_binding.PadsScl, padI2c);
        Mmio.Write32(_binding.IoSdaCtrl, _binding.Funcsel);
        Mmio.Write32(_binding.IoSclCtrl, _binding.Funcsel);
        ClearBus();
        Program();
    }

    void Program()
    {
        uint icClk = _binding.IcClkHz;
        uint rate = (uint)_busHz;
        uint period = (icClk + rate / 2) / rate;
        uint lcnt = period * 3 / 5;
        if (lcnt > 0xFFFF) lcnt = 0xFFFF;
        if (lcnt < 8) lcnt = 8;
        uint hcnt = period - lcnt;
        if (hcnt > 0xFFFF) hcnt = 0xFFFF;
        if (hcnt < 8) hcnt = 8;
        uint spklen = lcnt < 16 ? 1u : lcnt / 16;
        uint sdaHold = rate < 1000000
            ? icClk * 3 / 10000000 + 1
            : icClk * 3 / 25000000 + 1;
        if (sdaHold > lcnt - 2) sdaHold = lcnt - 2;

        uint con = Rp2350I2cLayout.IC_CON_MASTER_MODE
            | (Rp2350I2cLayout.SPEED_FAST << (int)Rp2350I2cLayout.IC_CON_SPEED_LSB)
            | Rp2350I2cLayout.IC_CON_IC_RESTART_EN
            | Rp2350I2cLayout.IC_CON_IC_SLAVE_DISABLE
            | Rp2350I2cLayout.IC_CON_TX_EMPTY_CTRL;

        Mmio.Write32(_enable, 0);
        Mmio.Write32(_con, con);
        Mmio.Write32(_rxTl, 0);
        Mmio.Write32(_txTl, 0);
        Mmio.Write32(_hcnt, hcnt);
        Mmio.Write32(_lcnt, lcnt);
        Mmio.Write32(_spklen, spklen);
        Mmio.Write32(_sdaHold, sdaHold);
        Mmio.Write32(_enable, _enableOn);
    }

    /// <summary>Whether IC_TAR may be programmed with <paramref name="address"/>: a 7-bit address
    /// outside the ranges the datasheet reserves, 0x00 to 0x07 and 0x78 to 0x7F, with which it
    /// does not guarantee correct operation.</summary>
    private static bool IsTargetAddress(int address)
    {
        return address >= 0x08 && address <= 0x77;
    }

    /// <summary>The strata's write sequence: START, address+W, <paramref name="count"/> bytes
    /// (STOP riding the last), each fed on TX-FIFO room; completion by TX_EMPTY, verdict from
    /// the abort source. A wait that runs out abandons the transfer and answers
    /// <see cref="I2cDriver.TimedOut"/>. An address outside 0x08-0x77 and a transfer of no bytes,
    /// which this controller cannot make, answer <see cref="I2cDriver.InvalidRequest"/> without
    /// touching the bus.</summary>
    public override int Write(int address, System.ReadOnlySpan<byte> buffer, int count)
    {
        if (!IsTargetAddress(address) || count < 1) return InvalidRequest;
        if (!Begin(address)) return GiveUp();
        for (int i = 0; i < count; i++)
        {
            if (!WaitTxRoom()) return GiveUp();
            uint cmd = (uint)(buffer[i] & 0xFF);
            if (i == count - 1) cmd |= _cmdStop;
            Mmio.Write32(_dataCmd, cmd);
        }
        return FinishTransaction();
    }

    /// <summary>The strata's read sequence: START, address+R, <paramref name="count"/> bytes in
    /// command/pop lockstep (STOP riding the last command). A wait that runs out abandons the
    /// transfer and answers <see cref="I2cDriver.TimedOut"/>, whatever the buffer holds. An
    /// address outside 0x08-0x77 and a read of no bytes answer
    /// <see cref="I2cDriver.InvalidRequest"/> without touching the bus.</summary>
    public override int Read(int address, System.Span<byte> buffer, int count)
    {
        if (!IsTargetAddress(address) || count < 1) return InvalidRequest;
        if (!Begin(address)) return GiveUp();
        for (int i = 0; i < count; i++)
        {
            int value = ClockOneRead(i == count - 1, false);
            if (value == ReadAborted) return AbortStatus();
            if (value == ReadStalled) return GiveUp();
            buffer[i] = (byte)value;
        }
        return FinishTransaction();
    }

    /// <summary>The strata's write_then_read sequence: the write bytes go WITHOUT stop, the
    /// first read command carries RESTART (the repeated start), the last carries STOP. A wait
    /// that runs out abandons the transfer and answers <see cref="I2cDriver.TimedOut"/>. An
    /// address outside 0x08-0x77 and a read half of no bytes, which would leave the bus held
    /// with no STOP, answer <see cref="I2cDriver.InvalidRequest"/> without touching the
    /// bus.</summary>
    public override int WriteRead(int address, System.ReadOnlySpan<byte> writeBuffer, int writeCount,
                                  System.Span<byte> readBuffer, int readCount)
    {
        if (!IsTargetAddress(address) || readCount < 1) return InvalidRequest;
        if (!Begin(address)) return GiveUp();
        for (int i = 0; i < writeCount; i++)
        {
            if (!WaitTxRoom()) return GiveUp();
            Mmio.Write32(_dataCmd, (uint)(writeBuffer[i] & 0xFF));
        }
        for (int i = 0; i < readCount; i++)
        {
            int value = ClockOneRead(i == readCount - 1, i == 0);
            if (value == ReadAborted) return AbortStatus();
            if (value == ReadStalled) return GiveUp();
            readBuffer[i] = (byte)value;
        }
        return FinishTransaction();
    }

    bool Begin(int address)
    {
        if ((Mmio.Read32(_resetsDone) & _blockReset) != _blockReset) return false;
        Mmio.Write32(_enable, 0);
        if (!WaitClear(_enableStatus, _enabledNow)) return false;
        Mmio.Read32(_clrTxAbrt);
        Mmio.Read32(_clrStopDet);
        Mmio.Write32(_tar, (uint)address);
        Mmio.Write32(_enable, _enableOn);
        return true;
    }

    bool WaitTxRoom()
    {
        return WaitSet(_status, _statusTfnf);
    }

    bool WaitSet(uint register, uint bit)
    {
        uint start = Now();
        while ((Mmio.Read32(register) & bit) == 0u)
        {
            if (Now() - start > _waitUs) return (Mmio.Read32(register) & bit) != 0u;
        }
        return true;
    }

    bool WaitClear(uint register, uint bit)
    {
        uint start = Now();
        while ((Mmio.Read32(register) & bit) != 0u)
        {
            if (Now() - start > _waitUs) return (Mmio.Read32(register) & bit) == 0u;
        }
        return true;
    }

    int ClockOneRead(bool last, bool restart)
    {
        if (!WaitTxRoom()) return ReadStalled;
        uint cmd = _cmdRead;
        if (restart) cmd |= _cmdRestart;
        if (last) cmd |= _cmdStop;
        Mmio.Write32(_dataCmd, cmd);
        uint start = Now();
        bool late = false;
        while (true)
        {
            if ((Mmio.Read32(_rawIntrStat) & _intTxAbrt) != 0u) return ReadAborted;
            if (Mmio.Read32(_rxflr) != 0u)
            {
                return (int)(Mmio.Read32(_dataCmd) & 0xFFu);
            }
            if (late) return ReadStalled;
            late = Now() - start > _waitUs;
        }
    }

    int FinishTransaction()
    {
        if (!WaitSet(_rawIntrStat, _intTxEmpty)) return GiveUp();
        int status = AbortStatus();
        if (!WaitSet(_rawIntrStat, _intStopDet)) return GiveUp();
        Mmio.Read32(_clrStopDet);
        return status;
    }

    int GiveUp()
    {
        if (CycleBlock())
        {
            ClearBus();
            Program();
        }
        return TimedOut;
    }

    bool CycleBlock()
    {
        Mmio.Write32(_resetsSet, _blockReset);
        uint start = Now();
        while ((Mmio.Read32(_resetsDone) & _blockReset) != 0u)
        {
            if (Now() - start > _waitUs) break;
        }
        Mmio.Write32(_resetsClr, _binding.ResetMask);
        start = Now();
        while ((Mmio.Read32(_resetsDone) & _binding.ResetMask) != _binding.ResetMask)
        {
            if (Now() - start > _waitUs) return false;
        }
        return true;
    }

    void ClearBus()
    {
        if (PadHigh(_sdaStatus)) return;
        uint start = Now();
        Mmio.Write32(_binding.IoSclCtrl, _pinReleased);
        bool clocking = AwaitPadHigh(_sclStatus, start);
        for (int pulse = 0; clocking && pulse < 9 && !PadHigh(_sdaStatus); pulse++)
        {
            Pause(ClearHalfPeriodUs);
            Mmio.Write32(_binding.IoSclCtrl, _pinLow);
            Pause(ClearHalfPeriodUs);
            Mmio.Write32(_binding.IoSclCtrl, _pinReleased);
            clocking = AwaitPadHigh(_sclStatus, start);
        }
        if (clocking)
        {
            Pause(ClearHalfPeriodUs);
            Mmio.Write32(_binding.IoSclCtrl, _pinLow);
            Pause(ClearHalfPeriodUs);
            Mmio.Write32(_binding.IoSdaCtrl, _pinLow);
            Pause(ClearHalfPeriodUs);
            Mmio.Write32(_binding.IoSclCtrl, _pinReleased);
            if (AwaitPadHigh(_sclStatus, start)) Pause(ClearHalfPeriodUs);
            Mmio.Write32(_binding.IoSdaCtrl, _pinReleased);
            Pause(ClearHalfPeriodUs);
        }
        Mmio.Write32(_binding.IoSdaCtrl, _binding.Funcsel);
        Mmio.Write32(_binding.IoSclCtrl, _binding.Funcsel);
    }

    bool PadHigh(uint pinStatus)
    {
        return (Mmio.Read32(pinStatus) & _padHigh) != 0u;
    }

    bool AwaitPadHigh(uint pinStatus, uint clearStart)
    {
        while (!PadHigh(pinStatus))
        {
            if (Now() - clearStart > ClearBudgetUs) return PadHigh(pinStatus);
        }
        return true;
    }

    void Pause(uint microseconds)
    {
        uint start = Now();
        while (Now() - start < microseconds)
        {
        }
    }

    uint Now()
    {
        return Mmio.Read32(_timerRaw);
    }

    void RequireClock()
    {
        uint first = Now();
        for (int poll = 0; poll < ClockPolls; poll++)
        {
            if (Now() != first) return;
        }
        throw new InvalidOperationException(
            "the RP2350's TIMER0 is not counting, so I2C waits cannot be timed");
    }

    int AbortStatus()
    {
        uint source = Mmio.Read32(_txAbrtSource);
        if (source == 0u) return Ok;
        Mmio.Read32(_clrTxAbrt);
        if ((source & _abrtAddrNoack) != 0u) return AddressNack;
        if ((source & _abrtDataNoack) != 0u) return DataNack;
        return OtherError;
    }
}
