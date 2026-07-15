// Lamella System.Device.Spi -- the dotnet/iot SPI API, in the System.Device.Gpio assembly.
namespace System.Device.Spi
{
    /// <summary>The connection settings of a device on a SPI bus.</summary>
    public sealed class SpiConnectionSettings
    {
        private int _busId;
        private int _chipSelectLine;
        private System.Device.Gpio.PinValue _chipSelectLineActiveState;
        private int _clockFrequency;
        private int _dataBitLength;
        private DataFlow _dataFlow;
        private SpiMode _mode;

        /// <summary>Initializes a new instance of the settings for the device on
        /// <paramref name="busId"/> using <paramref name="chipSelectLine"/> as the
        /// chip-select line (-1 for a raw bus / hardware-framed chip select).</summary>
        public SpiConnectionSettings(int busId, int chipSelectLine)
        {
            _busId = busId;
            _chipSelectLine = chipSelectLine;
            _chipSelectLineActiveState = System.Device.Gpio.PinValue.Low;
            _clockFrequency = 1000000;
            _dataBitLength = 8;
            _dataFlow = DataFlow.MsbFirst;
            _mode = SpiMode.Mode0;
        }

        /// <summary>The bus ID the device is connected to.</summary>
        public int BusId
        {
            get { return _busId; }
            set { _busId = value; }
        }

        /// <summary>The chip select line used on the bus, or -1 when the bus runs raw
        /// (hardware-framed or externally managed chip select).</summary>
        public int ChipSelectLine
        {
            get { return _chipSelectLine; }
            set { _chipSelectLine = value; }
        }

        /// <summary>Which value on the chip select pin means "active". Low by default.</summary>
        public System.Device.Gpio.PinValue ChipSelectLineActiveState
        {
            get { return _chipSelectLineActiveState; }
            set { _chipSelectLineActiveState = value; }
        }

        /// <summary>The frequency, in hertz, at which data is transferred.</summary>
        public int ClockFrequency
        {
            get { return _clockFrequency; }
            set { _clockFrequency = value; }
        }

        /// <summary>The word length, in bits, of the data transferred.</summary>
        public int DataBitLength
        {
            get { return _dataBitLength; }
            set { _dataBitLength = value; }
        }

        /// <summary>The order in which bits are transferred on the bus.</summary>
        public DataFlow DataFlow
        {
            get { return _dataFlow; }
            set { _dataFlow = value; }
        }

        /// <summary>The SPI mode (clock polarity and phase) being used.</summary>
        public SpiMode Mode
        {
            get { return _mode; }
            set { _mode = value; }
        }

        internal SpiConnectionSettings Clone()
        {
            SpiConnectionSettings copy = new SpiConnectionSettings(_busId, _chipSelectLine);
            copy._chipSelectLineActiveState = _chipSelectLineActiveState;
            copy._clockFrequency = _clockFrequency;
            copy._dataBitLength = _dataBitLength;
            copy._dataFlow = _dataFlow;
            copy._mode = _mode;
            return copy;
        }
    }
}
