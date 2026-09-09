// A driver for the ST LSM6DSOX six-axis inertial module: find it, configure it, read it.
//
// EVERY REGISTER NUMBER, ADDRESS, MASK, SHIFT AND EXPECTED VALUE COMES FROM `Lsm6dsoxPart`, which is
// generated from `parts/lsm6dsox/lsm6dsox.toml`. Nothing about the part is retyped here, so this
// driver and the table cannot drift: if the table is wrong this is wrong in the same way, which is
// what makes the table worth checking.
//
// WHAT IS HAND-WRITTEN IS THE ARITHMETIC AND THE ORDER -- the two things a table deliberately does
// not generate. A part's registers are facts; turning six bytes into an acceleration, and knowing
// that a configuration write must precede a read, is code.
//
// THE C# DRIVER BESIDE THIS ONE IS THE SOURCE FOR EVERYTHING THE TABLE DOES NOT CARRY.
// `parts/lsm6dsox/csharp/drivers/Lsm6dsox.cs` is written against this same table; the operating
// point, the sensitivity and the reasons for both are carried across from it rather than derived
// again here, so the two drivers agree by construction. The one place this one departs from it is
// noted below.
//
// THIS DRIVER CONFIGURES ONE OPERATING POINT AND SAYS SO. The part's rate and full-scale
// enumerations are each selected by a bit in ANOTHER register, so most codes name two different
// values -- which is why the table states neither enumeration. The one point used here is built from
// the two codes that mean the same thing whichever way those other bits are set, so this driver
// needs no knowledge the table refuses to carry. Anything wanting a different rate or range has to
// resolve that ambiguity first.

/// One reading of the accelerometer, in milli-g on each axis.
@frozen public struct Acceleration {
    /// Milli-g along X.
    public let x: Int32
    /// Milli-g along Y.
    public let y: Int32
    /// Milli-g along Z.
    public let z: Int32

    /// The magnitude of the vector, in milli-g.
    ///
    /// THE ONE FIGURE THAT CHECKS ITSELF. A body at rest reads about 1000 here whatever its
    /// orientation, so a caller can tell a working sensor from a plausible-looking wrong one without
    /// a reference instrument and without knowing which way up the part is mounted -- which matters,
    /// because a module on a cable has no defined orientation.
    public var milliG: Int32 {
        Acceleration.integerSquareRoot(x * x + y * y + z * z)
    }

    // Newton's method: these targets have no floating-point unit and need none here.
    static func integerSquareRoot(_ n: Int32) -> Int32 {
        if n <= 0 { return 0 }
        var x = n
        var y = (x + 1) / 2
        while y < x {
            x = y
            y = (x + n / x) / 2
        }
        return x
    }
}

/// The LSM6DSOX's own facts, in Swift's terms.
public enum Lsm6dsox {
    /// The strap this board's carrier ties SA0 to decides the address.
    ///
    /// THE ADDRESS IS REQUIRED RATHER THAN DEFAULTED. The part states two and a carrier picks one by
    /// where it tied a pin, so a default here would be this driver guessing at a fact only the board
    /// knows.
    @inline(__always)
    public static func address(sa0High: Bool) -> UInt8 {
        UInt8(truncatingIfNeeded: sa0High ? Lsm6dsoxPart.ADDRESS_STRAP_SA0_HIGH
                                          : Lsm6dsoxPart.ADDRESS_STRAP_SA0_LOW)
    }

    public static var identityRegister: PartRegister {
        PartRegister(UInt8(truncatingIfNeeded: Lsm6dsoxPart.IDENTITY_REG), .readOnly)
    }

    /// The value a genuine part answers with. One, here -- but still a set, because a driver that
    /// tests equality cannot be reused for a sibling that accepts several.
    public static var identity: IdentitySet {
        IdentitySet(UInt8(truncatingIfNeeded: Lsm6dsoxPart.IDENTITY_VALUE_0))
    }

    /// This part speaks I2C with the register address unchanged on the wire.
    public static var overI2c: BusProfile { BusProfile(.i2c, read: .identity, write: .identity) }

    public static var accelerometerControl: PartRegister {
        PartRegister(UInt8(truncatingIfNeeded: Lsm6dsoxPart.CTRL1_XL_REG), .readWrite)
    }
    public static var status: PartRegister {
        PartRegister(UInt8(truncatingIfNeeded: Lsm6dsoxPart.STATUS_REG_REG), .readOnly)
    }
    /// The first of the six consecutive accelerometer output registers.
    public static var accelerationBlock: PartRegister {
        PartRegister(UInt8(truncatingIfNeeded: Lsm6dsoxPart.OUTX_L_A_REG), .readOnly)
    }

    /// How many bytes one acceleration frame occupies.
    public static var burstLength: Int { Int(Lsm6dsoxPart.BURST_LENGTH) }

    /// The output-data-rate field of `CTRL1_XL`.
    public static var outputDataRate: RegisterField {
        RegisterField(mask: UInt8(truncatingIfNeeded: Lsm6dsoxPart.CTRL1_XL_ODR_XL),
                      shift: UInt8(truncatingIfNeeded: Lsm6dsoxPart.CTRL1_XL_ODR_XL_LSB))
    }
    /// The full-scale field of `CTRL1_XL`.
    public static var fullScale: RegisterField {
        RegisterField(mask: UInt8(truncatingIfNeeded: Lsm6dsoxPart.CTRL1_XL_FS_XL),
                      shift: UInt8(truncatingIfNeeded: Lsm6dsoxPart.CTRL1_XL_FS_XL_LSB))
    }
    /// The "accelerometer data available" bit of `STATUS_REG`.
    public static var accelerationReadyBit: RegisterField {
        RegisterField(mask: UInt8(truncatingIfNeeded: Lsm6dsoxPart.STATUS_REG_XLDA),
                      shift: UInt8(truncatingIfNeeded: Lsm6dsoxPart.STATUS_REG_XLDA_LSB))
    }
}

/// The ST LSM6DSOX over a register bus that can burst-read.
///
/// The bus seam reports no errors, so a wiring fault and an absent part are the same observation
/// here. `identify` returns the value it saw for exactly that reason -- see below.
public struct Lsm6dsoxDriver<Bus: BurstRegisterBus> {
    /// ODR code 0001 and full-scale code 00: the two that name ONE value in both columns of their
    /// tables -- 12.5 Hz and plus or minus 2 g -- which is what lets the sensitivity below be a
    /// single number.
    private static var odrCode12Hz5: UInt8 { 0x1 }
    private static var fullScaleCode2G: UInt8 { 0x0 }

    /// DS12814 Rev 4 mechanical characteristics: 0.061 mg/LSB at plus or minus 2 g, carried as
    /// nano-g so the conversion stays in integers.
    private static var nanoGPerLsb: Int32 { 61_000 }

    private let bus: Bus
    private let profile: BusProfile

    public init(bus: Bus, over profile: BusProfile) {
        self.bus = bus
        self.profile = profile
    }

    /// Whether the part on the other end is an LSM6DSOX, and the value it actually answered with.
    ///
    /// ON A MISMATCH THE VALUE READ IS RETURNED, not merely a false. A rejected part reads as no
    /// part at all, and the number is what tells a wrong part from an empty address.
    public func identify() -> (isPresent: Bool, received: UInt8) {
        let register = Lsm6dsox.identityRegister
        let value = bus.readRegister(profile.readTransform.apply(to: register.address))
        return (Lsm6dsox.identity.contains(value), value)
    }

    /// Brings the accelerometer out of power-down at 12.5 Hz, plus or minus 2 g.
    ///
    /// REQUIRED BEFORE ANY READING. The part powers up with its rate field zero, which is
    /// power-down: without this the output registers hold zero and the part reports no error, so an
    /// unconfigured device looks like a stationary one in free fall.
    ///
    public func configure() {
        let register = Lsm6dsox.accelerometerControl
        var value: UInt8 = 0
        value = Lsm6dsox.outputDataRate.insert(Self.odrCode12Hz5, into: value)
        value = Lsm6dsox.fullScale.insert(Self.fullScaleCode2G, into: value)
        bus.writeRegister(profile.writeTransform.apply(to: register.address), value)
    }

    /// Whether a new accelerometer sample is waiting.
    public func accelerationReady() -> Bool {
        let register = Lsm6dsox.status
        let value = bus.readRegister(profile.readTransform.apply(to: register.address))
        return Lsm6dsox.accelerationReadyBit.extract(from: value) != 0
    }

    /// Reads one acceleration sample, or `nil` if the bus returned a short frame.
    ///
    /// ONE BURST FROM THE ACCELERATION BLOCK, never six single reads: the axes must come from one
    /// conversion, and byte-at-a-time reads can straddle two. The burst also depends on the
    /// interface auto-increment bit, which this part sets at reset -- without it every axis comes
    /// back equal to X, which is a plausible frame rather than a failure.
    public func readAcceleration() -> Acceleration? {
        // Six bytes on the stack. A fixed-size tuple cannot allocate under any optimization, which
        // an array literal can -- see `IdentitySet` in PartKit for where that was measured.
        var frame: (UInt8, UInt8, UInt8, UInt8, UInt8, UInt8) = (0, 0, 0, 0, 0, 0)
        let register = Lsm6dsox.accelerationBlock
        let onTheWire = profile.readTransform.apply(to: register.address)
        let read: Int = withUnsafeMutableBytes(of: &frame) { raw in
            guard let base = raw.baseAddress else { return 0 }
            let buffer = UnsafeMutableBufferPointer<UInt8>(
                start: base.assumingMemoryBound(to: UInt8.self),
                count: raw.count
            )
            return bus.readRegisters(onTheWire, into: buffer)
        }
        guard read >= Lsm6dsox.burstLength else { return nil }
        return Acceleration(
            x: Self.toMilliG(low: frame.0, high: frame.1),
            y: Self.toMilliG(low: frame.2, high: frame.3),
            z: Self.toMilliG(low: frame.4, high: frame.5)
        )
    }

    /// Little-endian, two's complement, then scaled.
    ///
    /// The cast through `Int16` is what makes the sign right: treating the pair as unsigned turns
    /// half of every axis into a large positive.
    private static func toMilliG(low: UInt8, high: UInt8) -> Int32 {
        let raw = Int16(bitPattern: UInt16(high) << 8 | UInt16(low))
        return (Int32(raw) * nanoGPerLsb) / 1_000_000
    }
}
