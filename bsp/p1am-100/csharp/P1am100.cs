// Lamella.Boards.AutomationDirect.P1am100 -- the P1AM-100, an industrial PLC CPU (ATSAMD21G18A,
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.AutomationDirect
{
    public sealed class P1am100
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = P1am100Bindings.BOARD_MODEL;

        /// <summary>The yellow user LED on the faceplate -- the board's blink target, and the one
        /// indicator a program owns. The PWR and BASE lights beside it are hardware status and
        /// reach no pin.</summary>
        public static readonly int LedPin =
            LogicalPin(P1am100Bindings.LED_PORT_BASE, P1am100Bindings.LED_PIN);

        /// <summary>The recessed RUN / STOP toggle behind the faceplate. READS LOW IN THE RUN
        /// POSITION -- the switch ties this pad to ground on the RUN side and to 3.3 V on the STOP
        /// side -- so the pin wants <see cref="PinMode.Input"/> and a program comparing against
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>NOTHING IN HARDWARE ACTS ON THIS SWITCH. It is an input like any other, so what
        /// RUN means for a machine is the program's to decide and no part of this board enforces
        /// it.</remarks>
        public static readonly int RunSwitchPin =
            LogicalPin(P1am100Bindings.RUN_SWITCH_PORT_BASE, P1am100Bindings.RUN_SWITCH_PIN);

        /// <summary>Whether the RUN / STOP toggle is in the RUN position, read through the bound
        /// GPIO driver. Present so that the board answers the question rather than every caller
        /// re-deriving the polarity from <see cref="RunSwitchPin"/> and getting it backwards.</summary>
        public bool IsRunSelected()
        {
            GpioController pins = new GpioController();
            if (!pins.IsPinOpen(RunSwitchPin))
            {
                pins.OpenPin(RunSwitchPin, PinMode.Input);
            }
            return pins.Read(RunSwitchPin) == PinValue.Low;
        }

        /// <summary>The base controller's ENABLE line, ACTIVE HIGH: driving it
        /// <see cref="PinValue.High"/> starts the co-processor that owns every rack channel, and
        /// <see cref="PinValue.Low"/> disables it.</summary>
        public static readonly int BaseEnablePin =
            LogicalPin(P1am100Bindings.BASE_ENABLE_PORT_BASE, P1am100Bindings.BASE_ENABLE_PIN);

        /// <summary>The base controller's chip select on the SPI link, ACTIVE LOW. Brought out to
        /// the MKR header as A3, so a shield that claims A3 claims the rack with it.</summary>
        public static readonly int BaseChipSelectPin =
            LogicalPin(P1am100Bindings.BASE_CS_PORT_BASE, P1am100Bindings.BASE_CS_PIN);

        /// <summary>The base controller's readiness line back to this part, ACTIVE HIGH. An INPUT:
        /// the co-processor raises it, and a program that talks to the base without waiting on it
        /// is talking to something that has not finished starting.</summary>
        public static readonly int BaseReadyPin =
            LogicalPin(P1am100Bindings.BASE_READY_PORT_BASE, P1am100Bindings.BASE_READY_PIN);

        /// <summary>The microSD socket's chip select. The card rides SERCOM2; this is the one line
        /// of that group the board drives as a plain pin.</summary>
        /// <remarks>The board file records no asserted level for this line, so there is no
        /// polarity constant to lift and none is invented here.</remarks>
        public static readonly int SdChipSelectPin =
            LogicalPin(P1am100Bindings.SD_CS_PORT_BASE, P1am100Bindings.SD_CS_PIN);

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="P1am100"/> at all is what arms it, which is why a program constructs the
        /// board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static P1am100()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samd21GpioDriver(); }

        /// <summary>The family PORT driver this board bound, over every pin on the part.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while the
        /// facade talks to the first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the
        /// full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAMD21 PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The driver's logical numbering: group * 32 + pin, with the group taken from the
        /// binding's port base. Kept here rather than in the driver because it converts a BOARD fact
        /// (which port a device sits on) into the family's pin space.</summary>
        static int LogicalPin(uint portBase, uint pin)
        {
            int group = portBase == Samd21Instances.PORTB_BASE ? 1 : 0;
            return group * 32 + (int)pin;
        }
    }
}
