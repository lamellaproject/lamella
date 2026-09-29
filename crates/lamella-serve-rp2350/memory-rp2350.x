/* RP2350 boards (Raspberry Pi Pico 2 and Pico 2 W, Pimoroni Pico Plus 2 and Plus 2 W; dual
   Cortex-M33 / ARMv8-M): executes in place from XIP flash at 0x10000000; 520 KB SRAM (use the
   512 KB main window, stack at its top). The RP2350 bootrom scans the first 4 KB of flash for a
   PICOBIN block loop (the IMAGE_DEF) and boots the vector table at the image base -- so the
   vector table is first and the IMAGE_DEF block follows it, both inside the first 4 KB. The
   IMAGE_DEF is a single self-looping block marking an Arm RP2350 EXE (datasheet 5.9.5.1).

   This file is a template: build.rs substitutes @LAMELLA_FLASH_LENGTH@ and writes the result
   into OUT_DIR, because the flash a firmware may occupy is not the flash the part has. A
   firmware built with `serve` shares the flash with the deployed-image region, so it gets that
   region's base as its ceiling and one that outgrows it fails at link time; a firmware that
   takes no deploys owns the board's whole flash, which build.rs reads from the board's facts.
   Without the bound, an oversized firmware links cleanly and is then silently overwritten by
   the first deploy -- the failure this ceiling exists to refuse. build.rs derives the ceiling
   from IMAGE_BASE and refuses a base it cannot read, so the two spellings of one fact cannot
   drift. */
MEMORY
{
  FLASH (rx) : ORIGIN = 0x10000000, LENGTH = @LAMELLA_FLASH_LENGTH@
  RAM  (rwx) : ORIGIN = 0x20000000, LENGTH = 512K
}

ENTRY(reset)
EXTERN(fault)
EXTERN(IMAGE_DEF)

SECTIONS
{
  /* ARMv8-M core vector table at the image base -- the bootrom sets SP = [0], PC = [1]. */
  .vector_table ORIGIN(FLASH) :
  {
    LONG(0x20080000);                                           /* 0     initial SP (top of 512 KB) */
    LONG(reset | 1);                                            /* 1     Reset */
    LONG(fault | 1);                                            /* 2     NMI */
    LONG(fault | 1);                                            /* 3     HardFault */
    LONG(fault | 1); LONG(fault | 1); LONG(fault | 1);          /* 4-6   Mem/Bus/Usage */
    LONG(fault | 1);                                            /* 7     SecureFault */
    LONG(0); LONG(0); LONG(0);                                  /* 8-10  reserved */
    LONG(fault | 1);                                            /* 11    SVCall */
    LONG(fault | 1);                                            /* 12    DebugMon */
    LONG(0);                                                    /* 13    reserved */
    LONG(fault | 1);                                            /* 14    PendSV */
    LONG(fault | 1);                                            /* 15    SysTick */
  } > FLASH

  /* The PICOBIN IMAGE_DEF block, right after the vector table (well inside the first 4 KB). */
  .embedded_block : ALIGN(4)
  {
    KEEP(*(.embedded_block))
  } > FLASH

  .text : { *(.text .text.*) } > FLASH
  .rodata : { *(.rodata .rodata.*) } > FLASH

  /* The startup code (lamella-serve-core's startup.rs) copies .data from _sidata and zeroes
     .bss before main. */
  .data : ALIGN(4)
  {
    _sdata = .;
    *(.data .data.*);
    . = ALIGN(4);
    _edata = .;
  } > RAM AT > FLASH
  _sidata = LOADADDR(.data);

  .bss (NOLOAD) : ALIGN(4)
  {
    _sbss = .;
    *(.bss .bss.*);
    *(COMMON);
    . = ALIGN(4);
    _ebss = .;
  } > RAM

  /* Retained across a warm reset and not zeroed by startup (it sits past _ebss): the fault /
     panic handler stamps a magic here and resets, and the next boot reads it to come back up
     waiting for the host instead of re-running a crashing deployed app. */
  .noinit (NOLOAD) : ALIGN(4) { *(.noinit .noinit.*) } > RAM

  /DISCARD/ : { *(.ARM.exidx .ARM.exidx.*) *(.ARM.attributes) }
}
