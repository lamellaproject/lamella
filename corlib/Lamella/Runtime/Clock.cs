// Lamella managed corlib (from scratch). -- Lamella.Runtime.Clock
namespace Lamella.Runtime
{
    public static class Clock
    {
        [Lamella.Runtime.RuntimeProvided] public static void SetTicks(long utcTicks) { }

        [Lamella.Runtime.RuntimeProvided] public static bool IsSet() { return false; }
    }
}
