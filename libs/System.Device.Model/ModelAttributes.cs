// System.Device.Model -- dotnet/iot device-model annotation attributes (metadata-only, no runtime behavior): each tags a driver class or member so an IoT Plug-and-Play modeler can describe the device.
namespace System.Device.Model
{
    /// <summary>The device-model interface a class implements.</summary>
    [System.AttributeUsage(System.AttributeTargets.Class, AllowMultiple = false, Inherited = true)]
    public class InterfaceAttribute : System.Attribute
    {
        private readonly string _displayName;

        public InterfaceAttribute(string displayName) { _displayName = displayName; }

        public string DisplayName { get { return _displayName; } }
    }

    /// <summary>A sub-component property that references an interface.</summary>
    [System.AttributeUsage(System.AttributeTargets.Property, AllowMultiple = false, Inherited = true)]
    public class ComponentAttribute : System.Attribute
    {
        private readonly string _name;

        /// <summary>Names the component.</summary>
        /// <param name="name">The component's name, or null to name it after the property it is
        /// applied to.</param>
        public ComponentAttribute(string name = null) { _name = name; }

        /// <summary>The component's name, or null to use the property's own name.</summary>
        public string Name { get { return _name; } }
    }

    /// <summary>A property of the interface.</summary>
    [System.AttributeUsage(System.AttributeTargets.Method | System.AttributeTargets.Property, AllowMultiple = false, Inherited = true)]
    public class PropertyAttribute : System.Attribute
    {
        private readonly string _name;
        private readonly string _displayName;

        /// <summary>Names the property and its display name.</summary>
        /// <param name="name">The property's name in the interface, or null to name it after the
        /// member it is applied to.</param>
        /// <param name="displayName">The property's display name, or null to infer one.</param>
        public PropertyAttribute(string name = null, string displayName = null)
        {
            _name = name;
            _displayName = displayName;
        }

        /// <summary>The property's name in the interface, or null to use the member's own name.</summary>
        public string Name { get { return _name; } }

        /// <summary>The property's display name, or null to infer one.</summary>
        public string DisplayName { get { return _displayName; } }
    }

    /// <summary>Telemetry emitted by the interface.</summary>
    [System.AttributeUsage(System.AttributeTargets.Method | System.AttributeTargets.Property, AllowMultiple = false, Inherited = true)]
    public class TelemetryAttribute : System.Attribute
    {
        private readonly string _name;
        private readonly string _displayName;

        /// <summary>Names the telemetry and its display name.</summary>
        /// <param name="name">The telemetry's name in the interface, or null to name it after the
        /// member it is applied to.</param>
        /// <param name="displayName">The telemetry's display name, or null to infer one.</param>
        /// <remarks>Applied to a method, name the telemetry explicitly.</remarks>
        public TelemetryAttribute(string name = null, string displayName = null)
        {
            _name = name;
            _displayName = displayName;
        }

        /// <summary>The telemetry's name in the interface, or null to use the member's own name.</summary>
        public string Name { get { return _name; } }

        /// <summary>The telemetry's display name, or null to infer one.</summary>
        public string DisplayName { get { return _displayName; } }
    }

    /// <summary>A command the interface exposes.</summary>
    [System.AttributeUsage(System.AttributeTargets.Method, AllowMultiple = false, Inherited = true)]
    public class CommandAttribute : System.Attribute
    {
        private readonly string _name;
        private readonly string _displayName;

        /// <summary>Names the command and its display name.</summary>
        /// <param name="name">The command's name in the interface, or null to name it after the
        /// method it is applied to.</param>
        /// <param name="displayName">The command's display name, or null to infer one.</param>
        public CommandAttribute(string name = null, string displayName = null)
        {
            _name = name;
            _displayName = displayName;
        }

        /// <summary>The command's name in the interface, or null to use the method's own name.</summary>
        public string Name { get { return _name; } }

        /// <summary>The command's display name, or null to infer one.</summary>
        public string DisplayName { get { return _displayName; } }
    }
}
