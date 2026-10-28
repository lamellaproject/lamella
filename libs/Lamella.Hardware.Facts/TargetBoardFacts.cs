// Lamella.Hardware.Facts -- what Lamella.Hardware.TargetBoard asks of the board a program was built for, in a build that names no board.
namespace Lamella.Generated
{
    /// <summary>The facts of a build for no board: no name, and nothing to arm.</summary>
    internal sealed class TargetBoardFacts
    {
        private TargetBoardFacts() { }

        /// <summary>The board's id; null, as the program was built for no board.</summary>
        internal static string Name
        {
            get { return null; }
        }

        /// <summary>Arms nothing, as there is no board, and answers 0.</summary>
        internal static int Arm()
        {
            return 0;
        }
    }
}
