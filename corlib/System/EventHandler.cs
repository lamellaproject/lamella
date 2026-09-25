// Lamella managed corlib (from scratch). -- System.EventHandler and System.EventHandler<TEventArgs>
namespace System
{
    /// <summary>A method that handles an event whose data is an <see cref="EventArgs"/>.</summary>
    /// <param name="sender">The object that raised the event.</param>
    /// <param name="e">The event's data.</param>
    public delegate void EventHandler(object sender, EventArgs e);
}

#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    /// <summary>A method that handles an event whose data is a <typeparamref name="TEventArgs"/>.</summary>
    /// <typeparam name="TEventArgs">The type of the event's data.</typeparam>
    /// <param name="sender">The object that raised the event.</param>
    /// <param name="e">The event's data.</param>
#if LAMELLA_SURFACE_NETFX_4_5
    public delegate void EventHandler<TEventArgs>(object sender, TEventArgs e);
#else
    public delegate void EventHandler<TEventArgs>(object sender, TEventArgs e) where TEventArgs : EventArgs;
#endif
}
#endif
