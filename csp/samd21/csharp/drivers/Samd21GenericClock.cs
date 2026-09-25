// Routes a SAMD21 peripheral's generic clock from a clock generator, in the order the datasheet
// gives for a clock that may already be running: a running clock is stopped, and seen to stop,
// before its generator changes (DS40001882D 15.6.3.3).
//
// WHAT READS BACK, AND WHAT DOES NOT. CLKCTRL reads indirectly: an 8-bit write of a clock's ID selects
// it, and the next read returns its setting (15.6.4.1). The generator reads back as written. CLKEN
// reads 1 only while the peripheral is enabled and requesting its clock, so a clock just routed to an
// idle peripheral reads 0: the routing is confirmed by its generator, and CLKEN is never waited on
// to read 1. Waiting for it to read 0 after a disable is sound.
using Lamella.Generated;
using Lamella.Hardware;

internal sealed class Samd21GenericClock
{
    private Samd21GenericClock() { }

    // A bound on the wait for a running clock to stop, so one that never does is reported rather
    // than waited on forever.
    const int WaitBound = 100000;

    /// <summary>Routes the generic clock that <paramref name="clkctrl"/> names from the generator it
    /// names, and enables it.</summary>
    /// <param name="clkctrl">A GCLK.CLKCTRL word: the clock's ID, the generator, and CLKEN.</param>
    /// <returns>True once the clock reads back routed from that generator. False when the clock is
    /// locked to another generator, or when a running clock did not stop within the wait bound.</returns>
    internal static bool Route(uint clkctrl)
    {
        uint register = Samd21Instances.GCLK_BASE + Samd21GclkLayout.CLKCTRL_OFF;
        uint idAndGenerator = Samd21GclkLayout.CLKCTRL_ID | Samd21GclkLayout.CLKCTRL_GEN;
        uint source = clkctrl & idAndGenerator;

        Mmio.Write8(register, (byte)(clkctrl & Samd21GclkLayout.CLKCTRL_ID));
        uint current = Mmio.Read16(register);
        bool fromSource = (current & idAndGenerator) == source;
        // A locked clock ignores every write until a power reset (15.6.3.4).
        if ((current & Samd21GclkLayout.CLKCTRL_WRTLOCK) != 0u)
        {
            return fromSource;
        }
        if ((current & Samd21GclkLayout.CLKCTRL_CLKEN) != 0u)
        {
            if (fromSource)
            {
                return true;
            }
            Mmio.Write16(register, (ushort)(current & ~Samd21GclkLayout.CLKCTRL_CLKEN));
            if (!WaitStopped(register))
            {
                return false;
            }
        }
        Mmio.Write16(register, (ushort)source);
        Mmio.Write16(register, (ushort)(source | Samd21GclkLayout.CLKCTRL_CLKEN));
        // The last write selected this clock, so the read describes it.
        return (Mmio.Read16(register) & idAndGenerator) == source;
    }

    // CLKEN keeps reading its previous state until the disable has synchronized (15.6.3.2).
    static bool WaitStopped(uint register)
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read16(register) & Samd21GclkLayout.CLKCTRL_CLKEN) == 0u)
            {
                return true;
            }
        }
        return false;
    }
}
