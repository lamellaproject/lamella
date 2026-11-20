/* STM32F769NI (2 MB flash / 512 KB SRAM), the STM32F769I-DISCO's C# interpreter firmware.

   This is a file of its own rather than the F746's map with larger lengths, because the memory
   is laid out differently. RM0410 Rev 5 puts DTCM at 0x2000 0000 with 128 KB (the F746's has 64),
   then SRAM1 (368 KB) at 0x2002 0000 and SRAM2 (16 KB) at 0x2007 C000 - 0x2007 FFFF, with the
   next address reserved. So 0x2000 0000 .. 0x2008 0000 is one unbroken 512 KB span.

   FLASH is bounded to the first 1 MB of the part's 2 MB, as the F746's map bounds its own: a
   firmware that outgrows it fails to link rather than being overwritten later, and the upper
   half is the deployed-image window below, which a deploy erases and programs.

   The bound does not depend on the part's bank mode. An F769's sector geometry is selected by
   the nDBANK option bit rather than by its order code (RM0410 Rev 5 section 3.3.1), so a sector
   table is a claim about a configured part, but a linker needs only a contiguous span from
   0x0800 0000, and both modes provide one.

   The external memories are deliberately absent, for the reason the F746's map gives: neither the
   SDRAM on the FMC nor the QSPI NOR exists until its controller has been configured at run time,
   so handing the linker a region it could place .data or .bss into produces a firmware that faults
   before main. */
MEMORY
{
  FLASH (rx) : ORIGIN = 0x08000000, LENGTH = 1024K
  RAM  (rwx) : ORIGIN = 0x20000000, LENGTH = 512K
}

/* The deployed-image window: single-bank sectors 8 to 11, 4 x 256 KB from 0x0810 0000 to the end
   of the part (RM0410 section 3.3.1). The FLASH region above ends exactly here, so the linker cannot
   place a firmware byte inside the window that a deploy erases, and this is the one place the
   window's address is written: the firmware reads both symbols as addresses. */
_simage = 0x08100000;
_image_len = 1024K;

/* USART1, the board's virtual COM port, is interrupt position 37 (RM0410 Rev 5 Table 46: vector
   address 0x0000 00D4 = 0x40 + 4 x 37). A firmware that receives on it defines `usart1_isr`; a
   firmware that does not gets `fault` in the slot, and never enables the line. */
PROVIDE(usart1_isr = fault);

/* The external interrupt controller's GPIO lines arrive on seven positions (RM0410 Rev 5 Table 46, whose
   rows each give a vector address of 0x40 + 4 x the position): EXTI0 to EXTI4 at 6 to 10, one a
   line, EXTI9_5 at 23 for lines 5 to 9 and EXTI15_10 at 40 for lines 10 to 15. All seven take one
   handler, `exti_isr`, which finds the pending lines itself; a firmware that does not define it gets
   `fault` in all seven, and never enables them. */
PROVIDE(exti_isr = fault);

ENTRY(reset)
EXTERN(fault)

SECTIONS
{
  .vector_table ORIGIN(FLASH) :
  {
    /* Exported so a firmware can point VTOR here without restating the table's address: Armv7-M
       B3.2.5 leaves VTOR's reset value to the implementation, and RM0410 does not state this
       part's. */
    _svector_table = .;
    /* Core exceptions 0-15. */
    LONG(ORIGIN(RAM) + LENGTH(RAM));                             /* 0     initial SP */
    LONG(reset | 1);                                             /* 1     Reset */
    LONG(fault | 1);                                             /* 2     NMI */
    LONG(fault | 1);                                             /* 3     HardFault */
    LONG(fault | 1); LONG(fault | 1); LONG(fault | 1);           /* 4-6   Mem/Bus/Usage */
    LONG(0); LONG(0); LONG(0); LONG(0);                          /* 7-10  reserved */
    LONG(fault | 1);                                             /* 11    SVCall */
    LONG(0); LONG(0);                                            /* 12-13 reserved */
    LONG(fault | 1);                                             /* 14    PendSV */
    LONG(fault | 1);                                             /* 15    SysTick */
    /* External interrupts start at 16. A slot with no handler named here stays zero: its line
       is never enabled, so the slot is never fetched. */
    . = ORIGIN(FLASH) + (16 + 6) * 4;
    LONG(exti_isr | 1);                                         /* IRQ 6 = EXTI0 */
    LONG(exti_isr | 1);                                         /* IRQ 7 = EXTI1 */
    LONG(exti_isr | 1);                                         /* IRQ 8 = EXTI2 */
    LONG(exti_isr | 1);                                         /* IRQ 9 = EXTI3 */
    LONG(exti_isr | 1);                                         /* IRQ 10 = EXTI4 */
    . = ORIGIN(FLASH) + (16 + 23) * 4;
    LONG(exti_isr | 1);                                         /* IRQ 23 = EXTI9_5 */
    . = ORIGIN(FLASH) + (16 + 37) * 4;
    LONG(usart1_isr | 1);                                       /* IRQ 37 = USART1 */
    . = ORIGIN(FLASH) + (16 + 40) * 4;
    LONG(exti_isr | 1);                                         /* IRQ 40 = EXTI15_10 */
  } > FLASH

  .text : { *(.text .text.*) } > FLASH
  .rodata : { *(.rodata .rodata.*) } > FLASH

  /* The startup (lamella-serve-core/src/startup.rs) copies .data from _sidata and zeroes .bss. */
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

  /* Retained across a warm reset and NOT zeroed by startup (it sits past _ebss). */
  .noinit (NOLOAD) : ALIGN(4) { *(.noinit .noinit.*) } > RAM

  /DISCARD/ : { *(.ARM.exidx .ARM.exidx.*) *(.ARM.attributes) }
}
