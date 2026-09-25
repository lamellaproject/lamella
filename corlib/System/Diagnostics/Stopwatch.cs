// Lamella managed corlib (from scratch). -- System.Diagnostics.Stopwatch
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Diagnostics
{
    /// <summary>Measures elapsed time, over one interval or the total of several.</summary>
    /// <remarks>
    /// A stopwatch is running or stopped. Starting it adds to the time it has already measured;
    /// stopping it freezes that total until it is started again or reset.
    /// </remarks>
    public class Stopwatch
    {
        /// <summary>How many timer ticks make a second.</summary>
        public static readonly long Frequency = TimeSpan.TicksPerSecond;

        /// <summary>Whether the timer is a high-resolution performance counter. It is not: it is the
        /// system's monotonic clock.</summary>
        public static readonly bool IsHighResolution = false;

        private long elapsed;
        private long startedAt;
        private bool running;

        /// <summary>A stopped stopwatch that has measured no time.</summary>
        public Stopwatch()
        {
        }

        /// <summary>A new stopwatch, already running.</summary>
        /// <returns>The stopwatch.</returns>
        public static Stopwatch StartNew()
        {
            Stopwatch stopwatch = new Stopwatch();
            stopwatch.Start();
            return stopwatch;
        }

        /// <summary>The timer's current reading, in ticks of <see cref="Frequency"/>.</summary>
        /// <returns>The reading.</returns>
        public static long GetTimestamp()
        {
            return Lamella.Runtime.Clock.MonotonicTicks();
        }

        /// <summary>Whether the stopwatch is measuring.</summary>
        public bool IsRunning
        {
            get { return running; }
        }

        /// <summary>The total time measured.</summary>
        public TimeSpan Elapsed
        {
            get { return new TimeSpan(ElapsedTicks); }
        }

        /// <summary>The total time measured, in whole milliseconds.</summary>
        public long ElapsedMilliseconds
        {
            get { return ElapsedTicks / TimeSpan.TicksPerMillisecond; }
        }

        /// <summary>The total time measured, in timer ticks.</summary>
        public long ElapsedTicks
        {
            get
            {
                long total = elapsed;
                if (running)
                {
                    total = total + (GetTimestamp() - startedAt);
                }
                return total;
            }
        }

        /// <summary>Starts measuring, or resumes; does nothing while already running.</summary>
        public void Start()
        {
            if (!running)
            {
                startedAt = GetTimestamp();
                running = true;
            }
        }

        /// <summary>Stops measuring, keeping the time measured; does nothing while stopped.</summary>
        public void Stop()
        {
            if (running)
            {
                elapsed = elapsed + (GetTimestamp() - startedAt);
                running = false;
            }
        }

        /// <summary>Stops measuring and discards the time measured.</summary>
        public void Reset()
        {
            elapsed = 0;
            running = false;
        }

#if LAMELLA_SURFACE_NETFX_4_0
        /// <summary>Discards the time measured and starts measuring again.</summary>
        public void Restart()
        {
            elapsed = 0;
            startedAt = GetTimestamp();
            running = true;
        }
#endif
    }
}
#endif
