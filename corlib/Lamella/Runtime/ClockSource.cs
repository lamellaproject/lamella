// Lamella managed corlib (from scratch). -- Lamella.Runtime.ClockSource
namespace Lamella.Runtime
{
    public enum ClockSource
    {
        Unset = 0,
        RealTimeClock = 1,
        Seed = 2,
        ToolingPush = 3,
        Network = 4,
        Application = 5,
    }
}
