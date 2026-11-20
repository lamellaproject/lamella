/* STM32U5A5ZJ (4 MB flash, 2,496 KB contiguous SRAM, Cortex-M33 / ARMv8-M Mainline), the
   NUCLEO-U5A5ZJ-Q's C# interpreter firmware.

   FLASH is bank 1. RM0456 Rev 7 Table 53 organizes this part's main memory as two banks of 2 MB,
   256 pages of 8 KB each: bank 1 from 0x0800 0000 and bank 2 from 0x0820 0000. The firmware is
   bounded to bank 1, so a firmware that outgrows it fails to LINK, and the deploy window below is
   all of bank 2. RM0456 section 7.3.5: an ongoing flash operation "does not block the CPU as long as
   the CPU does not access the same flash memory bank", so a deploy never stalls the code that is
   running it.

   RAM is the contiguous span the parts row records (csp/stm32u5a5/parts.toml): SRAM1, SRAM2, SRAM3
   and SRAM5, 2,496 KB from 0x2000 0000, measured on a NUCLEO-U5A5ZJ-Q and equal to RM0456 Table 7's
   sum for this sub-group. SRAM4 and the backup SRAM are not contiguous with it and are not used. */
MEMORY
{
  FLASH (rx) : ORIGIN = 0x08000000, LENGTH = 2048K
  RAM  (rwx) : ORIGIN = 0x20000000, LENGTH = 2496K
}

/* The deployed-image window: bank 2, 0x0820 0000 to the end of the part. The FLASH region above
   ends exactly here, so the linker cannot place a firmware byte inside the window a deploy erases,
   and this is the one place the window's address is written: the firmware reads both symbols as
   addresses. */
_simage = 0x08200000;
_image_len = 2048K;

/* USART1, the board's virtual COM port, is interrupt position 61 (RM0456 Rev 7 Table 186: vector
   address 0x0000 0134 = 0x40 + 4 x 61). A firmware that receives on it defines `usart1_isr`; one
   that does not gets `fault` in the slot, and never enables the line. */
PROVIDE(usart1_isr = fault);

/* The external interrupt controller's sixteen GPIO lines arrive on a position each, EXTI0 to EXTI15 at
   11 to 26 (RM0456 Rev 7 Table 186, whose rows each give a vector address of 0x40 + 4 x the
   position). All sixteen take one handler, `exti_isr`, which finds the pending lines itself; a firmware that
   does not define it gets `fault` in all sixteen, and never enables them. */
PROVIDE(exti_isr = fault);

ENTRY(reset)
EXTERN(fault)

SECTIONS
{
  .vector_table ORIGIN(FLASH) :
  {
    /* Exported so the firmware points VTOR here itself rather than relying on the boot address an
       option byte selects. */
    _svector_table = .;
    /* ARMv8-M core exceptions 0-15. */
    LONG(ORIGIN(RAM) + LENGTH(RAM));                             /* 0     initial SP */
    LONG(reset | 1);                                             /* 1     Reset */
    LONG(fault | 1);                                             /* 2     NMI */
    LONG(fault | 1);                                             /* 3     HardFault */
    LONG(fault | 1); LONG(fault | 1); LONG(fault | 1);           /* 4-6   MemManage/BusFault/UsageFault */
    LONG(fault | 1);                                             /* 7     SecureFault */
    LONG(0); LONG(0); LONG(0);                                   /* 8-10  reserved */
    LONG(fault | 1);                                             /* 11    SVCall */
    LONG(fault | 1);                                             /* 12    DebugMonitor */
    LONG(0);                                                     /* 13    reserved */
    LONG(fault | 1);                                             /* 14    PendSV */
    LONG(fault | 1);                                             /* 15    SysTick */
    /* External interrupts start at 16. A slot with no handler named here stays zero: its line
       is never enabled, so the slot is never fetched. */
    . = ORIGIN(FLASH) + (16 + 11) * 4;
    LONG(exti_isr | 1);                                         /* IRQ 11 = EXTI0 */
    LONG(exti_isr | 1);                                         /* IRQ 12 = EXTI1 */
    LONG(exti_isr | 1);                                         /* IRQ 13 = EXTI2 */
    LONG(exti_isr | 1);                                         /* IRQ 14 = EXTI3 */
    LONG(exti_isr | 1);                                         /* IRQ 15 = EXTI4 */
    LONG(exti_isr | 1);                                         /* IRQ 16 = EXTI5 */
    LONG(exti_isr | 1);                                         /* IRQ 17 = EXTI6 */
    LONG(exti_isr | 1);                                         /* IRQ 18 = EXTI7 */
    LONG(exti_isr | 1);                                         /* IRQ 19 = EXTI8 */
    LONG(exti_isr | 1);                                         /* IRQ 20 = EXTI9 */
    LONG(exti_isr | 1);                                         /* IRQ 21 = EXTI10 */
    LONG(exti_isr | 1);                                         /* IRQ 22 = EXTI11 */
    LONG(exti_isr | 1);                                         /* IRQ 23 = EXTI12 */
    LONG(exti_isr | 1);                                         /* IRQ 24 = EXTI13 */
    LONG(exti_isr | 1);                                         /* IRQ 25 = EXTI14 */
    LONG(exti_isr | 1);                                         /* IRQ 26 = EXTI15 */
    . = ORIGIN(FLASH) + (16 + 61) * 4;
    LONG(usart1_isr | 1);                                       /* IRQ 61 = USART1 */
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
  .noinit (NOLOAD) : ALIGN(4) { *(.noinit .noinit.*) _enoinit = .; } > RAM

  /* What the statics leave of RAM. The stack keeps the top 128 KB, below the initial SP, and the
     heap is every byte between the statics and the stack's floor. The heap is not a static, so
     startup does not zero it: startup runs at the 4 MHz reset clock, where zeroing 2 MB is 524,288
     word stores and would hold the board deaf for hundreds of milliseconds after every reset, before
     its UART is up. The firmware hands the floor to the core's stack-limit register (MSPLIM), so a
     stack that outgrows its share faults rather than writing into the heap: an instruction that
     moves SP below that limit raises a STKOF UsageFault (Armv8-M ARM, DDI 0553B.y, section B3.21).
     The heap's base meets the reclaiming heap's 16-byte block alignment. */
  _sheap = ALIGN(_enoinit, 16);
  _stack_floor = ORIGIN(RAM) + LENGTH(RAM) - 128K;
  ASSERT(_stack_floor > _sheap, "the statics reach into the stack's share of RAM")

  /DISCARD/ : { *(.ARM.exidx .ARM.exidx.*) *(.ARM.attributes) }
}
