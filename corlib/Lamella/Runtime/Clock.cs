// Lamella managed corlib (from scratch). -- Lamella.Runtime.Clock
namespace Lamella.Runtime
{
    public sealed class Clock
    {
        private Clock() { }

        [Lamella.Runtime.RuntimeProvided]
        [Lamella.Runtime.IntendedDefault]
        public static void SetTicks(long utcTicks) { }

        [Lamella.Runtime.RuntimeProvided]
        [Lamella.Runtime.IntendedDefault]
        public static bool IsSet() { return false; }
    }
}
