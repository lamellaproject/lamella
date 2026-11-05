// Two pads a board wires to one net, as the Pimoroni boards wire GP26 to GP28 to GP40 to GP42 through 1k: what
// one may do while the other holds that net. One pad may drive while the other reads, and two may read, but two
// may not drive, and two pulls may not oppose, so each of those is refused; a pad nothing holds is parked, high
// impedance with no pull, so its reset pull-down does not load the net.
using System;
using Lamella.Generated;
using Lamella.Hardware;

internal sealed class Rp2350TwinPads
{
    private Rp2350TwinPads() { }

    const uint Ctrl0 = Rp2350Instances.IO_BANK0_BASE + Rp2350IoBank0Layout.GPIO0_CTRL_OFF;
    const uint Pads0 = Rp2350Instances.PADS_BANK0_BASE + Rp2350PadsBank0Layout.GPIO0_OFF;

    static uint CtrlOf(int pad)
    {
        return Ctrl0 + Rp2350IoBank0Layout.GPIO_CTRL_STRIDE * (uint)pad;
    }

    static uint PadsOf(int pad)
    {
        return Pads0 + Rp2350PadsBank0Layout.GPIO_STRIDE * (uint)pad;
    }

    /// <summary>The pad whose function register is <paramref name="ioCtrl"/>.</summary>
    internal static int PadOf(uint ioCtrl)
    {
        return (int)((ioCtrl - Ctrl0) / Rp2350IoBank0Layout.GPIO_CTRL_STRIDE);
    }

    /// <summary>Whether a pad with these two register words holds its net: a function that can use it, or an
    /// input buffer on with the pad out of isolation, which is how an edge is armed on a pad given no mode.</summary>
    internal static bool Holds(uint ctrl, uint pads)
    {
        uint function = ctrl & Rp2350IoBank0Layout.GPIO0_CTRL_FUNCSEL;
        return function == Rp2350IoBank0Layout.FUNCSEL_SIO || function == Rp2350IoBank0Layout.FUNCSEL_PWM
            || (pads & (Rp2350PadsBank0Layout.GPIO0_IE | Rp2350PadsBank0Layout.GPIO0_ISO)) == Rp2350PadsBank0Layout.GPIO0_IE;
    }

    /// <summary>Whether the pad drives its net: a PWM slice has it, or the SIO has it with its output enabled.</summary>
    internal static bool Drives(int pad)
    {
        uint function = Mmio.Read32(CtrlOf(pad)) & Rp2350IoBank0Layout.GPIO0_CTRL_FUNCSEL;
        if (function == Rp2350IoBank0Layout.FUNCSEL_PWM)
        {
            return true;
        }
        if (function != Rp2350IoBank0Layout.FUNCSEL_SIO)
        {
            return false;
        }
        uint outputEnable = Rp2350Instances.SIO_BASE
            + (pad < 32 ? Rp2350SioLayout.GPIO_OE_OFF : Rp2350SioLayout.GPIO_HI_OE_OFF);
        return (Mmio.Read32(outputEnable) & (1u << (pad & 31))) != 0u;
    }

    /// <summary>Leaves the pad high-impedance with no pull, so it neither drives its net nor loads it.</summary>
    internal static void Park(int pad)
    {
        Mmio.Write32(CtrlOf(pad), Rp2350IoBank0Layout.FUNCSEL_NULL);
        Mmio.Write32(PadsOf(pad), Rp2350PadsBank0Layout.GPIO0_OD);
    }

    /// <summary>Parks the pad unless something holds its net through it.</summary>
    internal static void ParkIfIdle(int pad)
    {
        if (!Holds(Mmio.Read32(CtrlOf(pad)), Mmio.Read32(PadsOf(pad))))
        {
            Park(pad);
        }
    }

    /// <summary>Parks a pad whose pin is closing, unless a started PWM channel has it, which goes on using it
    /// and gives it back at its own stop.</summary>
    internal static void ParkUnlessPwm(int pad)
    {
        if ((Mmio.Read32(CtrlOf(pad)) & Rp2350IoBank0Layout.GPIO0_CTRL_FUNCSEL) != Rp2350IoBank0Layout.FUNCSEL_PWM)
        {
            Park(pad);
        }
    }

    /// <summary>Clears the way for <paramref name="pad"/> to take a mode or a PWM channel while
    /// <paramref name="twin"/> shares its net: a twin nothing holds is parked, and one that holds the net is
    /// left as it is, so one pad can read what the other drives.</summary>
    /// <param name="pad">The pad about to change.</param>
    /// <param name="twin">The pad the board wires to the same net.</param>
    /// <param name="drive">Whether <paramref name="pad"/> is about to drive the net.</param>
    /// <param name="pull">The pad's pull bit it is about to enable, or 0 for none.</param>
    /// <exception cref="InvalidOperationException"><paramref name="drive"/> is set and the twin drives the net, or
    /// the twin holds the pull opposite to <paramref name="pull"/>. Nothing has been written.</exception>
    internal static void Claim(int pad, int twin, bool drive, uint pull)
    {
        uint word = Mmio.Read32(PadsOf(twin));
        if (!Holds(Mmio.Read32(CtrlOf(twin)), word))
        {
            Park(twin);
            return;
        }
        bool driving = Drives(twin);
        if (drive && driving)
        {
            Rp2350Refusal.Refuse("GP# cannot drive: this board wires GP# and GP@ to one net, and GP@ is driving it; set GP@ to an input, stop its PWM channel or close it first",
                pad, twin, 0, ' ');
        }
        if (driving || pull == 0u)
        {
            return;
        }
        uint opposite = pull == Rp2350PadsBank0Layout.GPIO0_PUE ? Rp2350PadsBank0Layout.GPIO0_PDE : Rp2350PadsBank0Layout.GPIO0_PUE;
        if ((word & opposite) != 0u)
        {
            Rp2350Refusal.Refuse(pull == Rp2350PadsBank0Layout.GPIO0_PUE
                ? "GP# cannot take a pull-up: this board wires GP# and GP@ to one net, and GP@ holds a pull-down on it; set GP@ to a plain input or close it first"
                : "GP# cannot take a pull-down: this board wires GP# and GP@ to one net, and GP@ holds a pull-up on it; set GP@ to a plain input or close it first",
                pad, twin, 0, ' ');
        }
    }
}
