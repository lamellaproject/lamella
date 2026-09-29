//! The RP2350 engine's pure parts: the program family against the
//! datasheet's encodings and the registered word lists, a cycle model of
//! the program over every phase and both frame shapes, the settings
//! ladder, the control and pad words, the pin table.

use crate::gspi::Setting;
use crate::rp2350::{
    CTRL_SM0_CLKDIV_RESTART, CTRL_SM0_ENABLE, CTRL_SM0_RESTART, FDEBUG_SM0_ALL, FSTAT_RXEMPTY0,
    FSTAT_TXEMPTY0, FSTAT_TXFULL0, FUNCSEL_PIO0, FUNCSEL_SIO, GPIO24_LEVEL_HIGH, INSTR_DATA_INPUT,
    INSTR_PARK, Ladder, OEOVER_ENABLE, PAD_CLOCK_LINE, PAD_CONTROL_LINE, PAD_DATA_LINE,
    PHASE_COUNT, PHASE_DEFAULT, PHASE_FIRST, PHASE_LAST, PIN_CLOCK, PIN_DATA, PIN_POWER,
    PIN_SELECT, PROC0_INTE3, PROGRAM_WORDS, Program, bit_hz, clkdiv_word, divider, execctrl_word,
    gpio_ctrl, header_x, header_y, instr, ladder, pad, pinctrl_word, program, setting_of,
    shiftctrl_word,
};
use std::vec::Vec;

const PREFIX: [u16; 8] = [
    0x6020, 0x6040, 0xE081, 0x6301, 0x1343, 0x6301, 0xB042, 0xF280,
];

fn family() -> [(i8, &'static [u16]); 8] {
    [
        (0, &[0x020B, 0x5301, 0x020B, 0x0089, 0x0000]),
        (1, &[0x020C, 0xB042, 0x5201, 0x020C, 0x0089, 0x0000]),
        (2, &[0x020C, 0xB142, 0x5101, 0x020C, 0x0089, 0x0000]),
        (3, &[0x020C, 0xB242, 0x5001, 0x020C, 0x0089, 0x0000]),
        (4, &[0x020C, 0xB342, 0x4001, 0x010C, 0x0089, 0x0000]),
        (-1, &[0x010C, 0x4001, 0xB342, 0x010C, 0x0089, 0x0000]),
        (-2, &[0x000C, 0x4101, 0xB342, 0x000C, 0x0089, 0x0000]),
        (-3, &[0x000B, 0x4201, 0xB342, 0x0089, 0x0000]),
    ]
}

#[test]
fn the_program_family_is_the_registered_word_list() {
    for (phase, tail) in family() {
        let Program { words, len } = program(phase);
        assert_eq!(len as usize, 8 + tail.len(), "phase {phase}");
        assert_eq!(&words[..8], &PREFIX, "phase {phase}: the prefix");
        assert_eq!(&words[8..len as usize], tail, "phase {phase}: the body");
        assert!(len as usize <= PROGRAM_WORDS);
    }
    assert_eq!(program(-9), program(PHASE_FIRST), "clamped below");
    assert_eq!(program(9), program(PHASE_LAST), "clamped above");
    assert_eq!(PHASE_LAST, 4);
    assert_eq!(PHASE_COUNT, 8);
    assert_eq!(PHASE_DEFAULT, -2);
}

/// One word decoded by the datasheet's rules: the opcode in bits 15:13,
/// the side-set value in bit 12 and the delay in bits 11:8 for a one-bit
/// side-set, the operands below.
fn decode(word: u16, len: u8) {
    let opcode = word >> 13;
    let delay = (word >> 8) & 0xF;
    assert!(delay <= 3, "a delay the program never needs");
    let body = word & 0xFF;
    match opcode {
        0 => {
            let condition = (body >> 5) & 7;
            let address = body & 31;
            assert!(
                matches!(
                    condition,
                    instr::JMP_ALWAYS | instr::JMP_X_DEC | instr::JMP_Y_DEC
                ),
                "a jump condition the program never uses"
            );
            assert!(address < u16::from(len), "a jump inside the program");
        }
        2 => {
            assert_eq!((body >> 5) & 7, 0, "IN from the pins");
            assert_eq!(body & 31, 1, "one bit in");
        }
        3 => {
            let destination = (body >> 5) & 7;
            let count = body & 31;
            assert!(matches!(
                destination,
                instr::OUT_PINS | instr::OUT_X | instr::OUT_Y
            ));
            assert!(
                count == 1 || count == 0,
                "one bit to the pins or a whole word to X or Y"
            );
        }
        5 => assert_eq!(body, 0x42, "MOV Y, Y is the only MOV: the no-operation"),
        7 => {
            assert_eq!((body >> 5) & 7, instr::SET_PINDIRS);
            assert!(body & 31 <= 1);
        }
        _ => panic!("an opcode the program never uses: {opcode}"),
    }
}

#[test]
fn every_word_of_every_program_decodes_by_the_datasheets_rules() {
    for phase in PHASE_FIRST..=PHASE_LAST {
        let Program { words, len } = program(phase);
        for word in &words[..len as usize] {
            decode(*word, len);
        }
        assert_eq!(words[len as usize..].iter().filter(|&&w| w != 0).count(), 0);
    }
    assert_eq!(instr::jmp(instr::JMP_Y_DEC, 9), 0x0089);
    assert_eq!(instr::in_pins(1), 0x4001);
    assert_eq!(instr::out(instr::OUT_X, 32), 0x6020);
    assert_eq!(instr::set(instr::SET_PINDIRS, 1), 0xE081);
    assert_eq!(instr::NOP, 0xA042);
    assert_eq!(INSTR_DATA_INPUT, 0xE080);
    assert_eq!(INSTR_PARK, 0x0000);
}

/// What the cycle model records of one frame.
struct Timeline {
    rising: Vec<u64>,
    outs: Vec<u64>,
    ins: Vec<u64>,
    drive: Option<u64>,
    release: Option<u64>,
    clock: Vec<bool>,
    end_clock: bool,
    end_output: bool,
}

/// The program run for a phase over a frame of `tx_words` out and
/// `rx_words` in, one instruction per cycle plus its delay, the side-set
/// applied on the instruction's first cycle, X and Y loaded from the two
/// header words, until the machine returns to instruction 0 with the
/// header words consumed (where it would stall on the empty FIFO).
fn model(phase: i8, tx_words: usize, rx_words: usize) -> Timeline {
    let Program { words, len } = program(phase);
    let words = &words[..len as usize];
    let headers = [header_x(tx_words), header_y(rx_words)];
    let mut header = 0usize;
    let (mut x, mut y) = (0u32, 0u32);
    let mut pc = 0usize;
    let mut cycle = 0u64;
    let mut clock = false;
    let mut output = false;
    let mut t = Timeline {
        rising: Vec::new(),
        outs: Vec::new(),
        ins: Vec::new(),
        drive: None,
        release: None,
        clock: Vec::new(),
        end_clock: false,
        end_output: false,
    };
    loop {
        if pc == 0 && header == 2 {
            break;
        }
        let word = words[pc];
        let side = (word >> 12) & 1 == 1;
        let delay = u64::from((word >> 8) & 0xF);
        if side && !clock {
            t.rising.push(cycle);
        }
        clock = side;
        let body = word & 0xFF;
        let mut next = pc + 1;
        match word >> 13 {
            0 => {
                let taken = match (body >> 5) & 7 {
                    instr::JMP_ALWAYS => true,
                    instr::JMP_X_DEC => {
                        let taken = x != 0;
                        x = x.wrapping_sub(1);
                        taken
                    }
                    instr::JMP_Y_DEC => {
                        let taken = y != 0;
                        y = y.wrapping_sub(1);
                        taken
                    }
                    other => panic!("condition {other}"),
                };
                if taken {
                    next = usize::from(body & 31);
                }
            }
            2 => t.ins.push(cycle),
            3 => match (body >> 5) & 7 {
                instr::OUT_X => {
                    x = headers[header];
                    header += 1;
                }
                instr::OUT_Y => {
                    y = headers[header];
                    header += 1;
                }
                _ => t.outs.push(cycle),
            },
            7 => {
                output = body & 31 != 0;
                if output {
                    t.drive = Some(cycle);
                } else {
                    t.release = Some(cycle);
                }
            }
            5 => {}
            other => panic!("opcode {other}"),
        }
        for _ in 0..=delay {
            t.clock.push(clock);
        }
        cycle += 1 + delay;
        pc = next;
        assert!(cycle < 100_000, "a runaway program");
    }
    t.end_clock = clock;
    t.end_output = output;
    t
}

fn check_frame(phase: i8, tx_words: usize, rx_words: usize) {
    let bits_out = tx_words * 32;
    let bits_in = rx_words * 32;
    let t = model(phase, tx_words, rx_words);
    let tag = std::format!("phase {phase}, {tx_words} words out, {rx_words} in");
    assert_eq!(t.outs.len(), bits_out, "{tag}: the bits out");
    assert_eq!(t.ins.len(), bits_in, "{tag}: the bits in");
    assert_eq!(
        t.rising.len(),
        bits_out + bits_in,
        "{tag}: one rising edge per bit"
    );
    let drive = t.drive.expect("the data line driven");
    assert!(drive < t.outs[0], "{tag}: driven before the first bit");
    for k in 0..bits_out {
        assert_eq!(
            t.rising[k],
            t.outs[k] + 4,
            "{tag}: bit {k} driven four cycles before its edge"
        );
        assert!(
            !t.clock[t.outs[k] as usize],
            "{tag}: bit {k} driven with the clock low"
        );
    }
    let last_command_edge = t.rising[bits_out - 1];
    assert_eq!(
        t.release,
        Some(last_command_edge + 1),
        "{tag}: the release one cycle after the last command edge"
    );
    for (k, &edge) in t.rising.iter().enumerate() {
        let e = edge as usize;
        assert!(
            t.clock[e..e + 4].iter().all(|&c| c),
            "{tag}: edge {k} high for four cycles"
        );
        assert!(!t.clock[e + 4], "{tag}: edge {k} falls after four cycles");
    }
    if bits_in > 0 {
        let first_read_edge = t.rising[bits_out];
        let expected_gap = if phase == -3 { 9 } else { 8 };
        assert_eq!(
            first_read_edge - last_command_edge,
            expected_gap,
            "{tag}: the entry cadence"
        );
        assert!(
            t.ins[0] >= last_command_edge + 6,
            "{tag}: the first sample past the chip's turn-on"
        );
        for k in 0..bits_in {
            let edge = t.rising[bits_out + k] as i64;
            assert_eq!(
                t.ins[k] as i64,
                edge + i64::from(phase),
                "{tag}: sample {k} at its phase"
            );
            if k > 0 {
                assert_eq!(
                    t.rising[bits_out + k] - t.rising[bits_out + k - 1],
                    8,
                    "{tag}: the read cadence"
                );
            }
        }
    }
    assert!(!t.end_clock, "{tag}: parked with the clock low");
    assert!(!t.end_output, "{tag}: parked with the data line an input");
}

#[test]
fn the_cycle_model_holds_for_every_phase_and_both_frame_shapes() {
    for phase in PHASE_FIRST..=PHASE_LAST {
        check_frame(phase, 2, 3);
        check_frame(phase, 1, 1);
        check_frame(phase, 2, 0);
        check_frame(phase, 1, 0);
    }
}

#[test]
fn the_header_words_count_the_bits() {
    assert_eq!(header_x(1), 30);
    assert_eq!(header_x(2), 62);
    assert_eq!(header_x(17), 542);
    assert_eq!(header_y(0), 0);
    assert_eq!(header_y(3), 96);
}

#[test]
fn the_settings_ladder_at_the_consumers_clock() {
    assert_eq!(
        ladder(150_000_000),
        Ladder {
            dividers: [3, 2, 1],
            count: 3
        }
    );
    assert_eq!(
        ladder(12_000_000),
        Ladder {
            dividers: [1, 0, 0],
            count: 1
        }
    );
    assert_eq!(
        ladder(100_000_000),
        Ladder {
            dividers: [2, 1, 0],
            count: 2
        }
    );
    assert_eq!(
        ladder(48_000_000),
        Ladder {
            dividers: [1, 0, 0],
            count: 1
        }
    );
    assert_eq!(divider(150_000_000, 6_250_000), 3);
    assert_eq!(divider(150_000_000, 9_375_000), 2);
    assert_eq!(divider(150_000_000, 18_750_000), 1);
    assert_eq!(divider(4_000_000_000, 1), 0xFFFF, "the field's ceiling");
    assert_eq!(bit_hz(150_000_000, 3), 6_250_000);
    assert_eq!(bit_hz(150_000_000, 2), 9_375_000);
    assert_eq!(bit_hz(150_000_000, 1), 18_750_000);
    assert_eq!(bit_hz(12_000_000, 1), 1_500_000);
    let expect = |step_ps| Setting {
        step_ps,
        steps_per_bit: 8,
        first: -3,
        phases: 8,
    };
    assert_eq!(setting_of(150_000_000, 3), expect(20_000));
    assert_eq!(setting_of(150_000_000, 2), expect(13_333));
    assert_eq!(setting_of(150_000_000, 1), expect(6_667));
    assert_eq!(setting_of(12_000_000, 1), expect(83_333));
    assert_eq!(setting_of(0, 1).step_ps, 0, "no clock, no setting");
    assert_eq!(setting_of(150_000_000, 3).period_ps(), 160_000);
    assert_eq!(setting_of(150_000_000, 3).last(), 4);
}

#[test]
fn the_control_words_and_the_pads() {
    assert_eq!(pinctrl_word(), 0x241C_7718);
    assert_eq!(shiftctrl_word(), 0x0003_0000);
    assert_eq!(execctrl_word(), 0x0001_F000);
    assert_eq!(clkdiv_word(3), 0x0003_0000);
    assert_eq!(clkdiv_word(1), 0x0001_0000);
    assert_eq!(
        CTRL_SM0_ENABLE | CTRL_SM0_RESTART | CTRL_SM0_CLKDIV_RESTART,
        0x0000_0111
    );
    assert_eq!(FSTAT_TXEMPTY0 | FSTAT_TXFULL0 | FSTAT_RXEMPTY0, 0x0101_0100);
    assert_eq!(FDEBUG_SM0_ALL, 0x0101_0101);
    assert_eq!(PAD_CONTROL_LINE, 0x52);
    assert_eq!(PAD_DATA_LINE, 0x56);
    assert_eq!(PAD_CLOCK_LINE, 0x75);
    assert_eq!(FUNCSEL_PIO0 | OEOVER_ENABLE, 0xC006);
    assert_eq!(FUNCSEL_SIO, 5);
    assert_eq!(PROC0_INTE3, 0x254);
    assert_eq!(GPIO24_LEVEL_HIGH, 0x2);
}

#[test]
fn the_pins_and_their_registers() {
    assert_eq!(
        (PIN_POWER, PIN_DATA, PIN_SELECT, PIN_CLOCK),
        (23, 24, 25, 29)
    );
    assert_eq!(gpio_ctrl(PIN_DATA), 0x4002_80C4);
    assert_eq!(gpio_ctrl(PIN_CLOCK), 0x4002_80EC);
    assert_eq!(pad(PIN_DATA), 0x4003_8064);
    assert_eq!(pad(PIN_CLOCK), 0x4003_8078);
    assert_eq!(pad(PIN_POWER), 0x4003_8060);
    assert_eq!(pad(PIN_SELECT), 0x4003_8068);
}
