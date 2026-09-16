//! The decoder, over encodings assembled by hand from the Armv7-M Architecture Reference Manual.

use super::*;

#[test]
fn each_stack_instruction_decodes_to_its_effect_and_width() {
    use Effect::*;
    let lr = 1u16 << 14;
    let pc = 1u16 << 15;
    let cases: &[(u16, u16, Effect, u32, &str)] = &[
        (0xB510, 0, Push { registers: (1 << 4) | lr, bytes: 8 }, 2, "push {r4, lr}"),
        (0xB580, 0, Push { registers: (1 << 7) | lr, bytes: 8 }, 2, "push {r7, lr}"),
        (0xB401, 0, Push { registers: 1, bytes: 4 }, 2, "push {r0}"),
        (0xE92D, 0x4FF0, Push { registers: 0x4FF0, bytes: 36 }, 4, "push.w {r4-r11, lr}"),
        (0xF84D, 0xED04, Push { registers: lr, bytes: 4 }, 4, "str.w lr, [sp, #-4]!"),
        (0xBD10, 0, Pop { registers: (1 << 4) | pc, bytes: 8 }, 2, "pop {r4, pc}"),
        (0xBC80, 0, Pop { registers: 1 << 7, bytes: 4 }, 2, "pop {r7}"),
        (0xE8BD, 0x8FF0, Pop { registers: 0x8FF0, bytes: 36 }, 4, "pop.w {r4-r11, pc}"),
        (0xE8BD, 0x4010, Pop { registers: (1 << 4) | lr, bytes: 8 }, 4, "pop.w {r4, lr}"),
        (0xF85D, 0xFB04, Pop { registers: pc, bytes: 4 }, 4, "ldr.w pc, [sp], #4"),
        (0xB082, 0, Reserve(8), 2, "sub sp, #8"),
        (0xB091, 0, Reserve(0x44), 2, "sub sp, #0x44"),
        (0xF1AD, 0x0D40, Reserve(0x40), 4, "sub.w sp, sp, #0x40"),
        (0xF5AD, 0x5D80, Reserve(0x1000), 4, "sub.w sp, sp, #0x1000"),
        (0xF2AD, 0x0D08, Reserve(8), 4, "subw sp, sp, #8"),
        (0xB002, 0, Release(8), 2, "add sp, #8"),
        (0xF10D, 0x0D10, Release(16), 4, "add.w sp, sp, #16"),
        (0xF20D, 0x0D08, Release(8), 4, "addw sp, sp, #8"),
        (0xF10D, 0x0B60, FramePointer { register: 11, offset: 0x60 }, 4, "add.w r11, sp, #0x60"),
        (0xF20D, 0x0B08, FramePointer { register: 11, offset: 8 }, 4, "addw r11, sp, #8"),
        (0xAF04, 0, FramePointer { register: 7, offset: 16 }, 2, "add r7, sp, #16"),
        (0x466F, 0, FramePointer { register: 7, offset: 0 }, 2, "mov r7, sp"),
        (0x46EB, 0, FramePointer { register: 11, offset: 0 }, 2, "mov r11, sp"),
        (0x46BD, 0, StackPointerFrom(7), 2, "mov sp, r7"),
        (0x46DD, 0, StackPointerFrom(11), 2, "mov sp, r11"),
        (0xED2D, 0x8B08, Push { registers: 0, bytes: 32 }, 4, "vpush {d8-d11}"),
        (0xED2D, 0x0A04, Push { registers: 0, bytes: 16 }, 4, "vpush {s0-s3}"),
        (0xECBD, 0x8B08, Pop { registers: 0, bytes: 32 }, 4, "vpop {d8-d11}"),
        (0x4770, 0, ReturnToLinkRegister, 2, "bx lr"),
        (0xE7FE, 0, Branch, 2, "b (16-bit)"),
        (0xF000, 0xB800, Branch, 4, "b.w"),
        (0xBF08, 0, IfThen, 2, "it eq"),
        (0xF7FF, 0xFFFE, Other, 4, "bl -- a call, which moves no stack"),
        (0xBF00, 0, Other, 2, "nop"),
        (0x4604, 0, Other, 2, "mov r4, r0"),
        (0x4700 | (3 << 3), 0, Other, 2, "bx r3 -- not the link register"),
        (0xF8D0, 0x0004, Other, 4, "ldr.w r0, [r0, #4] -- 32 bits, stepped over whole"),
        (0xE8BD, 0xC010, Other, 4, "pop.w with both pc and lr, which the manual makes UNPREDICTABLE"),
        (0xF1AD, 0x0240, Other, 4, "sub.w r2, sp, #0x40 -- not the stack pointer"),
    ];
    for &(first, second, expected, width, what) in cases {
        assert_eq!(decode(first, second), (expected, width), "{what}: {first:#06x} {second:#06x}");
    }
}

#[test]
fn a_modified_immediate_expands_as_the_manual_defines() {
    assert_eq!(thumb_expand_imm(0x0AB), Some(0x0000_00AB));
    assert_eq!(thumb_expand_imm(0x1AB), Some(0x00AB_00AB));
    assert_eq!(thumb_expand_imm(0x2AB), Some(0xAB00_AB00));
    assert_eq!(thumb_expand_imm(0x3AB), Some(0xABAB_ABAB));
    assert_eq!(thumb_expand_imm(0x100), None, "a repeated pattern of zero is UNPREDICTABLE");
    assert_eq!(thumb_expand_imm(0xD80), Some(0x0000_1000), "0x80 rotated right by 27");
    assert_eq!(thumb_expand_imm(0x47F), Some(0xFF00_0000), "0xFF rotated right by 8");
}
