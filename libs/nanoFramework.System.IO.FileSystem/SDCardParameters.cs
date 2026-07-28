// nanoFramework.System.IO.FileSystem (a nanoFramework compatibility assembly) -- the SDCard parameter objects.
namespace nanoFramework.System.IO.FileSystem
{
    /// <summary>Parameters for creating an SD card instance on a SPI bus.</summary>
    public class SDCardSpiParameters
    {
        /// <summary>The SPI bus the card is wired to.</summary>
        public uint spiBus;

        /// <summary>The chip-select pin the card is selected with.</summary>
        public uint chipSelectPin;

        /// <summary>Which card slot this is; slot 0 mounts as drive <c>D:</c>, slot 1 as <c>E:</c>,
        /// and so on.</summary>
        public uint slotIndex;
    }

    /// <summary>Parameters for creating an SD card instance on the MMC interface.</summary>
    public class SDCardMmcParameters
    {
        /// <summary>The data width to use on the MMC SD protocol.</summary>
        public SDCard.SDDataWidth dataWidth;

        /// <summary>Which card slot this is; slot 0 mounts as drive <c>D:</c>, slot 1 as <c>E:</c>,
        /// and so on.</summary>
        public uint slotIndex;
    }

    /// <summary>Parameters for detecting the presence of a card.</summary>
    public class CardDetectParameters
    {
        /// <summary>Whether a detected card is mounted automatically.</summary>
        public bool autoMount;

        /// <summary>Whether card presence is sensed through a GPIO pin.</summary>
        public bool enableCardDetectPin;

        /// <summary>The pin presence is sensed on, when
        /// <see cref="enableCardDetectPin"/> is set.</summary>
        public uint cardDetectPin;

        /// <summary>The level that pin reads when a card IS present.</summary>
        public bool cardDetectedState;
    }
}
