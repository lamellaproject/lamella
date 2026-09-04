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
    private readonly uint _status;
    private readonly uint _rxflr;
    private readonly uint _sdaHold;
    private readonly uint _txAbrtSource;
    private readonly uint _spklen;
    private readonly uint _resetsClr;
    private readonly uint _resetsDone;
    private readonly uint _cmdRead;
    private readonly uint _cmdStop;
    private readonly uint _cmdRestart;
    private readonly uint _intTxEmpty;
    private readonly uint _intTxAbrt;
    private readonly uint _intStopDet;
    private readonly uint _statusTfnf;
    private readonly uint _abrtAddrNoack;
    private readonly uint _abrtDataNoack;
    private readonly Rp2350I2cBinding _binding;

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
        _status = i2c + Rp2350I2cLayout.IC_STATUS_OFF;
        _rxflr = i2c + Rp2350I2cLayout.IC_RXFLR_OFF;
        _sdaHold = i2c + Rp2350I2cLayout.IC_SDA_HOLD_OFF;
        _txAbrtSource = i2c + Rp2350I2cLayout.IC_TX_ABRT_SOURCE_OFF;
        _spklen = i2c + Rp2350I2cLayout.IC_FS_SPKLEN_OFF;
        _resetsClr = Rp2350Instances.RESETS_CLR_BASE + Rp2350ResetsLayout.RESET_OFF;
        _resetsDone = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_DONE_OFF;
        _cmdRead = Rp2350I2cLayout.IC_DATA_CMD_CMD;
        _cmdStop = Rp2350I2cLayout.IC_DATA_CMD_STOP;
        _cmdRestart = Rp2350I2cLayout.IC_DATA_CMD_RESTART;
        _intTxEmpty = Rp2350I2cLayout.IC_RAW_INTR_STAT_TX_EMPTY;
        _intTxAbrt = Rp2350I2cLayout.IC_RAW_INTR_STAT_TX_ABRT;
        _intStopDet = Rp2350I2cLayout.IC_RAW_INTR_STAT_STOP_DET;
        _statusTfnf = Rp2350I2cLayout.IC_STATUS_TFNF;
        _abrtAddrNoack = Rp2350I2cLayout.IC_TX_ABRT_SOURCE_ABRT_7B_ADDR_NOACK;
        _abrtDataNoack = Rp2350I2cLayout.IC_TX_ABRT_SOURCE_ABRT_TXDATA_NOACK;
    }

    /// <summary>Brings the bound DW up as a 7-bit master at (approximately, never above)
    /// <paramref name="busHz"/>: reset release, pad prep (de-isolate + input buffer +
    /// internal pull-up + schmitt kept for the open-drain edges), pin routing, then the
    /// official pico-sdk counts against the binding's ic_clk, programmed disabled with
    /// enable last. Idempotent -- the facades re-run it freely.</summary>
    public override void Configure(int busHz)
    {
        Mmio.Write32(_resetsClr, _binding.ResetMask);
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_resetsDone) & _binding.ResetMask) == _binding.ResetMask) break;
        }

        uint padI2c = Rp2350PadsBank0Layout.GPIO0_IE | Rp2350PadsBank0Layout.GPIO0_PUE
            | Rp2350PadsBank0Layout.GPIO0_SCHMITT;
        Mmio.Write32(_binding.PadsSda, padI2c);
        Mmio.Write32(_binding.PadsScl, padI2c);
        Mmio.Write32(_binding.IoSdaCtrl, _binding.Funcsel);
        Mmio.Write32(_binding.IoSclCtrl, _binding.Funcsel);

        uint icClk = _binding.IcClkHz;
        uint period = (icClk + (uint)busHz / 2) / (uint)busHz;
        uint lcnt = period * 3 / 5;
        if (lcnt > 0xFFFF) lcnt = 0xFFFF;
        if (lcnt < 8) lcnt = 8;
        uint hcnt = period - lcnt;
        if (hcnt > 0xFFFF) hcnt = 0xFFFF;
        if (hcnt < 8) hcnt = 8;
        uint spklen = lcnt < 16 ? 1u : lcnt / 16;
        uint sdaHold = busHz < 1000000
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
        Mmio.Write32(_enable, 1);
    }

    /// <summary>The strata's write sequence: START, address+W, <paramref name="count"/> bytes
    /// (STOP riding the last), each fed on TX-FIFO room; completion by TX_EMPTY, verdict from
    /// the abort source.</summary>
    public override int Write(int address, System.ReadOnlySpan<byte> buffer, int count)
    {
        SetTarget(address);
        for (int i = 0; i < count; i++)
        {
            WaitTxRoom();
            uint cmd = (uint)(buffer[i] & 0xFF);
            if (i == count - 1) cmd |= _cmdStop;
            Mmio.Write32(_dataCmd, cmd);
        }
        return FinishTransaction();
    }

    /// <summary>The strata's read sequence: START, address+R, <paramref name="count"/> bytes in
    /// command/pop lockstep (STOP riding the last command).</summary>
    public override int Read(int address, System.Span<byte> buffer, int count)
    {
        SetTarget(address);
        for (int i = 0; i < count; i++)
        {
            int value = ClockOneRead(i == count - 1, false);
            if (value < 0) return AbortStatus();
            buffer[i] = (byte)value;
        }
        return FinishTransaction();
    }

    /// <summary>The strata's write_then_read sequence: the write bytes go WITHOUT stop, the
    /// first read command carries RESTART (the repeated start), the last carries STOP.</summary>
    public override int WriteRead(int address, System.ReadOnlySpan<byte> writeBuffer, int writeCount,
                                  System.Span<byte> readBuffer, int readCount)
    {
        SetTarget(address);
        for (int i = 0; i < writeCount; i++)
        {
            WaitTxRoom();
            Mmio.Write32(_dataCmd, (uint)(writeBuffer[i] & 0xFF));
        }
        for (int i = 0; i < readCount; i++)
        {
            int value = ClockOneRead(i == readCount - 1, i == 0);
            if (value < 0) return AbortStatus();
            readBuffer[i] = (byte)value;
        }
        return FinishTransaction();
    }

    void SetTarget(int address)
    {
        Mmio.Write32(_enable, 0);
        Mmio.Write32(_tar, (uint)(address & 0x7F));
        Mmio.Write32(_enable, 1);
    }

    void WaitTxRoom()
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_status) & _statusTfnf) != 0u) return;
        }
    }

    int ClockOneRead(bool last, bool restart)
    {
        WaitTxRoom();
        uint cmd = _cmdRead;
        if (restart) cmd |= _cmdRestart;
        if (last) cmd |= _cmdStop;
        Mmio.Write32(_dataCmd, cmd);
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_rawIntrStat) & _intTxAbrt) != 0u) return -1;
            if (Mmio.Read32(_rxflr) != 0u)
            {
                return (int)(Mmio.Read32(_dataCmd) & 0xFFu);
            }
        }
        return -1;
    }

    int FinishTransaction()
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_rawIntrStat) & _intTxEmpty) != 0u) break;
        }
        int status = AbortStatus();
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_rawIntrStat) & _intStopDet) != 0u) break;
        }
        Mmio.Read32(_clrStopDet);
        return status;
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
