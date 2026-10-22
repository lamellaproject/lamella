//! A board's flash outside the chip: the quad-SPI NOR flash a board solders to the chip's QUADSPI
//! controller, which the chip maps as memory once the controller is configured.
//!
//! **A BOARD FACT IN TWO HALVES.** Which pins carry the bus is the board's wiring, and which flash
//! part is soldered there is the board's bill of materials -- and a schematic may allow more than one
//! part on the same footprint. So a board states its wiring and the parts it allows, and a write
//! identifies the fitted one by its JEDEC identity before it erases anything.

use lamella_cmsis_dap_stm32::qspi::{
    MT25QL512ABB, MX25L51245G, N25Q128A, NorPart, QspiPin, QspiWiring, STM32F7_QUADSPI_WINDOW,
};

/// A board's external flash, as a write reaches it.
#[derive(Debug, PartialEq, Eq)]
pub struct ExternalFlash {
    /// The board's name for the region, as its facts call it.
    pub region: &'static str,
    /// Where the chip maps the flash once the controller is configured.
    pub window: u32,
    /// The pins the board wires to the controller.
    pub wiring: QspiWiring,
    /// Every part the board's schematic allows on the footprint.
    pub parts: &'static [&'static NorPart],
}

impl ExternalFlash {
    /// The largest of [`parts`](Self::parts): the most the window can hold on any unit of this
    /// board.
    #[must_use]
    pub fn largest(&self) -> u32 {
        self.parts.iter().map(|part| part.size).max().unwrap_or(0)
    }

    /// The parts this board allows, named, for a refusal.
    #[must_use]
    pub fn allowed(&self) -> String {
        let names: Vec<String> = self
            .parts
            .iter()
            .map(|part| {
                let [a, b, c] = part.identity;
                format!("{} ({a:02x} {b:02x} {c:02x})", part.name)
            })
            .collect();
        names.join(" or ")
    }
}

/// The external flash `board` carries, by its board id, where a write reaches one.
#[must_use]
pub fn for_board(board: &str) -> Option<&'static ExternalFlash> {
    match board {
        "st-stm32f746g-disco" => Some(&STM32F746G_DISCO_QSPI),
        "st-stm32f769i-disco" => Some(&STM32F769I_DISCO_QSPI),
        _ => None,
    }
}

/// Port numbers as [`QspiPin::port`] counts them.
const PORT_B: u8 = 1;
const PORT_C: u8 = 2;
const PORT_D: u8 = 3;
const PORT_E: u8 = 4;

/// The 32F746GDISCOVERY's quad-SPI flash.
///
/// The pins are UM1907 Rev 7's I/O assignment table: PB2 QUADSPI_CLK, PB6 QUADSPI_BK1_NCS, PD11
/// QUADSPI_BK1_IO0, PD12 QUADSPI_BK1_IO1, PE2 QUADSPI_BK1_IO2 and PD13 QUADSPI_BK1_IO3. The
/// function numbers are DS10916 Rev 5 Table 12's: AF9 for five of them and AF10 for PB6, whose AF9
/// cell is CAN2_TX. The part is the N25Q128A the board's schematic fits.
pub const STM32F746G_DISCO_QSPI: ExternalFlash = ExternalFlash {
    region: "qspi",
    window: STM32F7_QUADSPI_WINDOW,
    wiring: QspiWiring {
        pins: [
            QspiPin { port: PORT_B, pin: 2, function: 9 },
            QspiPin { port: PORT_B, pin: 6, function: 10 },
            QspiPin { port: PORT_D, pin: 11, function: 9 },
            QspiPin { port: PORT_D, pin: 12, function: 9 },
            QspiPin { port: PORT_E, pin: 2, function: 9 },
            QspiPin { port: PORT_D, pin: 13, function: 9 },
        ],
    },
    parts: &[&N25Q128A],
};

/// The 32F769IDISCOVERY's quad-SPI flash.
///
/// The pins are UM2033 Rev 5's I/O assignment table: PB2 QUADSPI_CLK, PB6 QUADSPI_BK1_NCS, PC9
/// QUADSPI_BK1_IO0, PC10 QUADSPI_BK1_IO1, PE2 QUADSPI_BK1_IO2 and PD13 QUADSPI_BK1_IO3. The
/// function numbers are DS11532 Rev 9 Table 13's: AF9 for five of them and AF10 for PB6, whose AF9
/// cell is CAN2_TX.
///
/// **TWO PARTS, ONE FOOTPRINT.** The MB1225 B03 schematic's quad-SPI sheet draws the Micron
/// MT25QL512ABB1EW9 (U12) and the Macronix MX25L51245GZ2I-08G (U250) on the same nets, so a unit
/// may carry either, and the write tells them apart by identity.
pub const STM32F769I_DISCO_QSPI: ExternalFlash = ExternalFlash {
    region: "qspi",
    window: STM32F7_QUADSPI_WINDOW,
    wiring: QspiWiring {
        pins: [
            QspiPin { port: PORT_B, pin: 2, function: 9 },
            QspiPin { port: PORT_B, pin: 6, function: 10 },
            QspiPin { port: PORT_C, pin: 9, function: 9 },
            QspiPin { port: PORT_C, pin: 10, function: 9 },
            QspiPin { port: PORT_E, pin: 2, function: 9 },
            QspiPin { port: PORT_D, pin: 13, function: 9 },
        ],
    },
    parts: &[&MT25QL512ABB, &MX25L51245G],
};
