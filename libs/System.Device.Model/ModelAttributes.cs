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

        /// <summary>Names the component after the property it is applied to.</summary>
        public ComponentAttribute() { }

        /// <summary>Names the component explicitly.</summary>
        public ComponentAttribute(string name) { _name = name; }

        /// <summary>The component's name, or null to use the property's own name.</summary>
        public string Name { get { return _name; } }
    }

    /// <summary>A property of the interface.</summary>
    [System.AttributeUsage(System.AttributeTargets.Method | System.AttributeTargets.Property, AllowMultiple = false, Inherited = true)]
    public class PropertyAttribute : System.Attribute
    {
        private readonly string _name;
        private readonly string _displayName;

        /// <summary>Names the property after the member it is applied to.</summary>
        public PropertyAttribute() { }

        /// <summary>Names the property explicitly, leaving the display name to be inferred.</summary>
        public PropertyAttribute(string name) { _name = name; }

        /// <summary>Names the property and its display name explicitly.</summary>
        public PropertyAttribute(string name, string displayName)
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

        /// <summary>Names the telemetry after the member it is applied to. Applied to a method, prefer naming it explicitly.</summary>
        public TelemetryAttribute() { }

        /// <summary>Names the telemetry explicitly, leaving the display name to be inferred.</summary>
        public TelemetryAttribute(string name) { _name = name; }

        /// <summary>Names the telemetry and its display name explicitly.</summary>
        public TelemetryAttribute(string name, string displayName)
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

        /// <summary>Names the command after the method it is applied to.</summary>
        public CommandAttribute() { }

        /// <summary>Names the command explicitly, leaving the display name to be inferred.</summary>
        public CommandAttribute(string name) { _name = name; }

        /// <summary>Names the command and its display name explicitly.</summary>
        public CommandAttribute(string name, string displayName)
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
