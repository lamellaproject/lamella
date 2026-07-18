// Lamella.Boards.Mkr1000 -- the Arduino MKR1000: the same ATSAMW25 module as the SAM W25 XPro
using Lamella.Generated;

namespace Lamella.Boards
{
    public sealed class Mkr1000
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = Mkr1000Bindings.BOARD_MODEL;
    }
}
