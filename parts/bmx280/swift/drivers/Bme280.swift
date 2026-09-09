// The BME280 in Swift's own terms: a typed projection of the part facts emitted beside this file.
//
// HAND-WRITTEN. It is the reference shape for a generated typed layer, so its structure is
// deliberately mechanical -- every value is read from `Bme280Part` and none is restated here, which
// is what lets a generator produce this for any part without the two drifting apart.
//
// ONE CLASS OF FACT IS EXEMPT FROM THAT CLAIM AND THE EXEMPTION IS FORCED. A behavioural fact is
// emitted as `StaticString` -- `BUS_SPI_REGISTER_READ_TRANSFORM` is `"set-bit7"` -- and a
// `StaticString` is not comparable in Embedded Swift, so `Bme280Part.BUS_SPI_REGISTER_READ_TRANSFORM
// == "set-bit7"` does not compile. `overSpi` below therefore NAMES `.setBit7` rather than reading
// it, and that is the one line here that can drift from the table. See `parts/common/swift/PartKit.swift`.
//
// The vocabulary this is written in -- `PartBus`, `RegisterTransform`, `PartRegister`,
// `IdentitySet`, `BusProfile`, `RegisterBus` -- lives in `parts/common/swift/PartKit.swift`, once
// for the whole tree. It used to live in this file, which made a second driver a second copy of it.

/// The BME280's own facts, in Swift's terms.
public enum Bme280 {
    /// The strap this board's carrier ties SDO to decides the address. The part states the range and
    /// a carrier fixes the strap, so this takes the strap rather than defaulting one.
    @inline(__always)
    public static func address(sdoHigh: Bool) -> UInt8 {
        UInt8(truncatingIfNeeded: sdoHigh ? Bme280Part.ADDRESS_STRAP_SDO_HIGH
                                          : Bme280Part.ADDRESS_STRAP_SDO_LOW)
    }

    public static var identityRegister: PartRegister {
        PartRegister(UInt8(truncatingIfNeeded: Bme280Part.IDENTITY_REG), .readOnly)
    }

    /// The value this part answers with. A sibling in the same family answers others, which is why
    /// this is a set rather than a single value.
    public static var identity: IdentitySet {
        IdentitySet(UInt8(truncatingIfNeeded: Bme280Part.IDENTITY_VALUE_0))
    }

    public static var overI2c: BusProfile { BusProfile(.i2c, read: .identity, write: .identity) }
    public static var overSpi: BusProfile { BusProfile(.spi, read: .setBit7, write: .clearBit7) }

    public static var resetRegister: PartRegister {
        PartRegister(UInt8(truncatingIfNeeded: Bme280Part.RESET_REG), .writeOnly)
    }
    public static var statusRegister: PartRegister {
        PartRegister(UInt8(truncatingIfNeeded: Bme280Part.STATUS_REG), .readOnly)
    }
}

/// What a part driver looks like when the facts carry their own meaning.
///
/// Compare with what the flat table forces: the caller reads `BUS_SPI_REGISTER_READ_TRANSFORM`,
/// discovers it cannot compare the string, hard-codes `| 0x80` in its own file, and the fact and
/// the behaviour drift apart from that moment on.
public struct Bme280Driver<Bus: RegisterBus> {
    private let bus: Bus
    private let profile: BusProfile

    public init(bus: Bus, over profile: BusProfile) {
        self.bus = bus
        self.profile = profile
    }

    /// Whether the part on the other end is a BME280.
    ///
    /// It returns the value it saw as well as the verdict. A bare `false` cannot tell "a different
    /// part answered" from "nothing answered", and on a bus those want different responses.
    public func identify() -> (isPresent: Bool, received: UInt8) {
        let register = Bme280.identityRegister
        let onTheWire = profile.readTransform.apply(to: register.address)
        let value = bus.readRegister(onTheWire)
        return (Bme280.identity.contains(value), value)
    }
}
