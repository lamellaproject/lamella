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

        public ComponentAttribute(string name) { _name = name; }

        public string Name { get { return _name; } }
    }

    /// <summary>A property of the interface.</summary>
    [System.AttributeUsage(System.AttributeTargets.Method | System.AttributeTargets.Property, AllowMultiple = false, Inherited = true)]
    public class PropertyAttribute : System.Attribute
    {
        private readonly string _name;
        private readonly string _displayName;

        public PropertyAttribute(string name, string displayName)
        {
            _name = name;
            _displayName = displayName;
        }

        public string Name { get { return _name; } }

        public string DisplayName { get { return _displayName; } }
    }

    /// <summary>Telemetry emitted by the interface.</summary>
    [System.AttributeUsage(System.AttributeTargets.Method | System.AttributeTargets.Property, AllowMultiple = false, Inherited = true)]
    public class TelemetryAttribute : System.Attribute
    {
        private readonly string _name;
        private readonly string _displayName;

        public TelemetryAttribute(string name, string displayName)
        {
            _name = name;
            _displayName = displayName;
        }

        public string Name { get { return _name; } }

        public string DisplayName { get { return _displayName; } }
    }

    /// <summary>A command the interface exposes.</summary>
    [System.AttributeUsage(System.AttributeTargets.Method, AllowMultiple = false, Inherited = true)]
    public class CommandAttribute : System.Attribute
    {
        private readonly string _name;
        private readonly string _displayName;

        public CommandAttribute(string name, string displayName)
        {
            _name = name;
            _displayName = displayName;
        }

        public string Name { get { return _name; } }

        public string DisplayName { get { return _displayName; } }
    }
}
