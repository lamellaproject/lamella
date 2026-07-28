// nanoFramework.System.IO.FileSystem (a nanoFramework compatibility assembly) -- SDCard.

using System;
namespace nanoFramework.System.IO.FileSystem
{
    /// <summary>Allows an SD memory card to be configured and mounted on the system.</summary>
    public class SDCard : IDisposable
    {
        /// <summary>Which interface an SD card is driven over.</summary>
        public enum SDInterfaceType
        {
            /// <summary>Interface already defined in firmware.</summary>
            System = 0,
            /// <summary>MMC SD card interface type.</summary>
            Mmc = 1,
            /// <summary>SPI SD card interface type.</summary>
            Spi = 2
        }

        /// <summary>The data width to use on the MMC SD protocol.</summary>
        public enum SDDataWidth
        {
            /// <summary>1-bit data width.</summary>
            _1_bit = 1,
            /// <summary>4-bit data width.</summary>
            _4_bit = 2
        }

        private SDInterfaceType _cardType;
        private uint _slotIndex;
        private SDCardSpiParameters _spiParameters;
        private SDCardMmcParameters _mmcParameters;
        private CardDetectParameters _cdParameters;
        private bool _disposed;

        /// <summary>Creates an SD card on the interface the firmware already defines, in slot
        /// 0.</summary>
        public SDCard()
        {
            Initialize(SDInterfaceType.System, 0, null);
        }

        /// <summary>Creates an SD card on the interface the firmware already defines, in
        /// <paramref name="slotIndex"/>.</summary>
        public SDCard(uint slotIndex)
        {
            Initialize(SDInterfaceType.System, slotIndex, null);
        }

        /// <summary>Creates an SD card on the MMC interface described by
        /// <paramref name="mmcParameters"/>.</summary>
        public SDCard(SDCardMmcParameters mmcParameters)
        {
            InitializeMmc(mmcParameters, null);
        }

        /// <summary>Creates an SD card on the MMC interface described by
        /// <paramref name="mmcParameters"/>, detected as <paramref name="cdParameters"/>
        /// describes.</summary>
        public SDCard(SDCardMmcParameters mmcParameters, CardDetectParameters cdParameters)
        {
            InitializeMmc(mmcParameters, cdParameters);
        }

        /// <summary>Creates an SD card on the SPI bus described by
        /// <paramref name="spiParameters"/>.</summary>
        public SDCard(SDCardSpiParameters spiParameters)
        {
            InitializeSpi(spiParameters, null);
        }

        /// <summary>Creates an SD card on the SPI bus described by
        /// <paramref name="spiParameters"/>, detected as <paramref name="cdParameters"/>
        /// describes.</summary>
        public SDCard(SDCardSpiParameters spiParameters, CardDetectParameters cdParameters)
        {
            InitializeSpi(spiParameters, cdParameters);
        }

        private void InitializeSpi(SDCardSpiParameters spiParameters, CardDetectParameters cdParameters)
        {
            if ((object)spiParameters == null) throw new ArgumentNullException("spiParameters");
            _spiParameters = spiParameters;
            Initialize(SDInterfaceType.Spi, spiParameters.slotIndex, cdParameters);
        }

        private void InitializeMmc(SDCardMmcParameters mmcParameters, CardDetectParameters cdParameters)
        {
            if ((object)mmcParameters == null) throw new ArgumentNullException("mmcParameters");
            _mmcParameters = mmcParameters;
            Initialize(SDInterfaceType.Mmc, mmcParameters.slotIndex, cdParameters);
        }

        private void Initialize(SDInterfaceType cardType, uint slotIndex, CardDetectParameters cdParameters)
        {
            if ((object)cdParameters != null)
            {
                if (cdParameters.enableCardDetectPin)
                    throw new NotSupportedException(
                        "Sensing card presence on a pin is not supported on this device.");
                if (cdParameters.autoMount)
                    throw new NotSupportedException(
                        "Mounting a card automatically on insertion is not supported on this device.");
            }
            _cardType = cardType;
            _slotIndex = slotIndex;
            _cdParameters = cdParameters;
        }

        /// <summary>The interface this card is driven over.</summary>
        public SDInterfaceType CardType
        {
            get { return _cardType; }
        }

        /// <summary>Which card slot this is. Slot 0 mounts as drive <c>D:</c>, slot 1 as <c>E:</c>,
        /// and so on.</summary>
        public uint SlotIndex
        {
            get { return _slotIndex; }
        }

        /// <summary>The SPI parameters this card was created with, or null if it was not created on
        /// a SPI bus.</summary>
        public SDCardSpiParameters SpiParameters
        {
            get { return _spiParameters; }
        }

        /// <summary>The MMC parameters this card was created with, or null if it was not created on
        /// the MMC interface.</summary>
        public SDCardMmcParameters MmcParameters
        {
            get { return _mmcParameters; }
        }

        /// <summary>The card-detect parameters this card was created with, or null if none were
        /// given.</summary>
        public CardDetectParameters CdParameters
        {
            get { return _cdParameters; }
        }

        /// <summary>Whether card presence is sensed on a pin. Always false here: a card created with
        /// pin detection is refused at construction.</summary>
        public bool CardDetectEnabled
        {
            get { return false; }
        }

        /// <summary>Whether a card is present. With no presence pin to read there is nothing to
        /// report an absence, so this is true.</summary>
        public bool IsCardDetected
        {
            get { return true; }
        }

        /// <summary>Whether this card is currently mounted.</summary>
        public bool IsMounted
        {
            get { return NativeSdCard.IsMounted(MountPoint()) != 0; }
        }

        /// <summary>Mounts the card, making its volume reachable through <c>System.IO</c> under this
        /// slot's drive letter.</summary>
        /// <exception cref="System.NotSupportedException">The card is on the MMC interface or on an
        /// interface the firmware defines, neither of which this device drives; or nothing native
        /// owns the SPI bus named by <see cref="SDCardSpiParameters.spiBus"/>.</exception>
        /// <exception cref="System.IO.IOException">The card could not be brought up, or carries no
        /// FAT volume.</exception>
        public void Mount()
        {
            ThrowIfDisposed();
            if (_cardType != SDInterfaceType.Spi)
                throw new NotSupportedException(
                    "Only an SD card on a SPI bus can be mounted on this device.");
            string mountPoint = MountPoint();
            int code = NativeSdCard.MountSdOverSpiBus(
                mountPoint,
                unchecked((int)_spiParameters.spiBus),
                unchecked((int)_spiParameters.chipSelectPin));
            if (code != 0) NativeSdCard.Throw(code, mountPoint);
        }

        /// <summary>Unmounts the card, releasing its volume.</summary>
        public void Unmount()
        {
            ThrowIfDisposed();
            NativeSdCard.Unmount(MountPoint());
        }

        /// <summary>Unmounts the card if it is mounted, and releases it.</summary>
        public void Dispose()
        {
            if (_disposed) return;
            _disposed = true;
            NativeSdCard.Unmount(MountPoint());
        }

        private string MountPoint()
        {
            char letter = (char)('D' + _slotIndex);
            return letter.ToString() + ":";
        }

        private void ThrowIfDisposed()
        {
            if (_disposed) throw new ObjectDisposedException("SDCard");
        }
    }
}
