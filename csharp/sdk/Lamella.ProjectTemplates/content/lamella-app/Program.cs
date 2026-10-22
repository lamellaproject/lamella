#if (hasBoard)
using System.Device.Gpio;
using System.Threading;

namespace App
{
    public static class Program
    {
        // The pin that drives the board's own LED, numbered as GpioController numbers the board's
        // pins.
        private const int LedPin = LAMELLA_LED_PIN;

        public static void Main()
        {
            GpioController controller = new GpioController();
            controller.OpenPin(LedPin, PinMode.Output);
            while (true)
            {
                controller.Write(LedPin, PinValue.High);
                Thread.Sleep(500);
                controller.Write(LedPin, PinValue.Low);
                Thread.Sleep(500);
            }
        }
    }
}
#else
using System;

namespace App
{
    public static class Program
    {
        public static void Main()
        {
            Console.WriteLine("Hello, World!");
        }
    }
}
#endif
