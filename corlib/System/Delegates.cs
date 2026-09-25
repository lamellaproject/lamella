// Lamella managed corlib (from scratch). -- System.Delegate / System.MulticastDelegate
namespace System
{
    public abstract class Delegate : ICloneable
    {
        private object _target;

        private IntPtr _methodPtr;

        [Lamella.Runtime.RuntimeProvided] public static Delegate Combine(Delegate a, Delegate b) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static Delegate Remove(Delegate source, Delegate value) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static bool operator ==(Delegate a, Delegate b) { return false; }
        [Lamella.Runtime.RuntimeProvided] public static bool operator !=(Delegate a, Delegate b) { return true; }

        /// <summary>Whether <paramref name="obj"/> is a delegate of the same type with the same targets,
        /// methods and invocation list.</summary>
        /// <param name="obj">The object to compare with this delegate.</param>
        /// <returns>True when the two are equal delegates.</returns>
        [Lamella.Runtime.RuntimeProvided] public override bool Equals(object obj) { return (object)this == obj; }

        /// <summary>A hash code for the delegate, equal for equal delegates.</summary>
        /// <returns>The hash code.</returns>
        [Lamella.Runtime.RuntimeProvided] public override int GetHashCode() { return 0; }

        /// <summary>A shallow copy of the delegate.</summary>
        /// <returns>A new delegate of the same type with the same invocation list.</returns>
        [Lamella.Runtime.RuntimeProvided] public virtual object Clone() { return null; }
    }

    public abstract class MulticastDelegate : Delegate
    {
        private Delegate[] _invocationList;

        /// <summary>Whether <paramref name="obj"/> is a delegate of exactly this type with the same
        /// invocation list.</summary>
        /// <param name="obj">The object to compare with this delegate.</param>
        /// <returns>True when the two are equal delegates.</returns>
        [Lamella.Runtime.RuntimeProvided] public sealed override bool Equals(object obj) { return (object)this == obj; }

        /// <summary>A hash code for the delegate, equal for equal delegates.</summary>
        /// <returns>The hash code.</returns>
        [Lamella.Runtime.RuntimeProvided] public sealed override int GetHashCode() { return 0; }
    }
}
