@ STM32F7 quad-SPI NOR flash loader, Thumb-2 (ARMv7E-M, Cortex-M7).
@
@ Erases 64 KB blocks, or programs pages, of the NOR flash behind the chip's QUADSPI controller, in
@ indirect mode, from code running in the chip's own SRAM.
@
@ WHY IT RUNS ON THE CORE: the controller's data register is a single address, and the registers
@ after it are the status-polling and timeout registers, so a probe's burst write cannot fill the
@ FIFO. A host writing it from outside spends one probe round trip per 32-bit word. Here the host
@ writes a whole buffer into SRAM in one burst, and this loop moves it into the FIFO a page at a
@ time.
@
@ IT KNOWS NO FLASH PART. Every command arrives as a finished QUADSPI_CCR word in the operation
@ block (built on the host from the identified part's own datasheet), so this one routine drives any
@ part whose page program and block erase take an address and report completion through a status
@ register's write-in-progress bit.
@
@ EVERY WAIT IS BOUNDED. A controller that never finishes a command returns STUCK, and a part that
@ never clears write-in-progress returns TIMEOUT, so a call always comes back with an answer the host
@ can name.
@
@ On entry (the ABI `call_target` sets up):
@   r0 = the operation block, in SRAM, word aligned. Words, written by the host before the call:
@     +0   QUADSPI register base
@     +4   operation: 1 = erase blocks, 2 = program
@     +8   device address of the first block, or of the first byte to program
@     +12  count: blocks to erase, or bytes to program
@     +16  source of the bytes to program, in SRAM
@     +20  CCR word: write enable (no address, no data)
@     +24  CCR word: read the status register (no address, one data byte, indirect read)
@     +28  CCR word: the operation itself (an address; no data for an erase, data for a program)
@     +32  status mask: write in progress
@     +36  status mask: write enable latch
@     +40  unit: the erase block, or the program page, in bytes (a power of two)
@     +44  status reads allowed per operation before giving up
@     +48  out: the device address the call stopped at
@     +52  out: the last status byte read
@ On exit:
@   r0 = 0 when the whole count is done, else one of the codes below.
@ The caller has brought up the controller and the pins, and has halted the core.

        .syntax unified
        .cpu    cortex-m7
        .thumb

        .equ    QSPI_SR,        0x08
        .equ    QSPI_FCR,       0x0C
        .equ    QSPI_DLR,       0x10
        .equ    QSPI_CCR,       0x14
        .equ    QSPI_AR,        0x18
        .equ    QSPI_DR,        0x20

        .equ    SR_TEF,         0x01    @ transfer error
        .equ    SR_TCF,         0x02    @ transfer complete
        .equ    SR_BUSY,        0x20
        .equ    FCR_CTCF,       0x02
        .equ    FCR_CLEAR,      0x1B    @ CTOF | CSMF | CTCF | CTEF
        .equ    FIFO_BYTES,     32      @ the indirect-mode FIFO

        .equ    B_BASE,         0
        .equ    B_OP,           4
        .equ    B_ADDRESS,      8
        .equ    B_COUNT,        12
        .equ    B_SOURCE,       16
        .equ    B_CCR_WREN,     20
        .equ    B_CCR_RDSR,     24
        .equ    B_CCR_OP,       28
        .equ    B_WIP,          32
        .equ    B_WEL,          36
        .equ    B_UNIT,         40
        .equ    B_POLLS,        44
        .equ    B_AT,           48
        .equ    B_STATUS,       52

        .equ    OP_ERASE,       1
        .equ    OP_PROGRAM,     2

        .equ    DONE,           0
        .equ    TIMEOUT,        1       @ the part never cleared write-in-progress
        .equ    TRANSFER_ERROR, 2       @ the controller flagged an address outside the part
        .equ    NOT_ENABLED,    3       @ write enable did not set the write enable latch
        .equ    STUCK,          4       @ the controller never finished a command
        .equ    BAD_OPERATION,  5       @ the operation word is neither erase nor program

        .global _start
        .thumb_func
_start:
        push    {r4-r11, lr}
        mov     r4, r0                  @ r4 = the operation block
        ldr     r5, [r4, #B_BASE]       @ r5 = QUADSPI
        ldr     r6, [r4, #B_ADDRESS]    @ r6 = the device address being worked on
        ldr     r7, [r4, #B_COUNT]      @ r7 = what is left
        ldr     r8, [r4, #B_SOURCE]     @ r8 = the next source byte
        ldr     r0, [r4, #B_OP]
        cmp     r0, #OP_ERASE
        beq     erase
        cmp     r0, #OP_PROGRAM
        beq     program
        movs    r0, #BAD_OPERATION
        b       finish

@ ---- erase: one block per pass ----
erase:
        cmp     r7, #0
        beq     done
        bl      write_enable
        cmp     r0, #0
        bne     finish
        bl      idle
        cmp     r0, #0
        bne     finish
        movs    r0, #FCR_CLEAR
        str     r0, [r5, #QSPI_FCR]
        ldr     r0, [r4, #B_CCR_OP]
        str     r0, [r5, #QSPI_CCR]
        str     r6, [r5, #QSPI_AR]      @ an address and no data: this write starts the command
        bl      complete
        cmp     r0, #0
        bne     finish
        bl      settle
        cmp     r0, #0
        bne     finish
        ldr     r0, [r4, #B_UNIT]
        add     r6, r6, r0
        subs    r7, r7, #1
        b       erase

@ ---- program: never across a page boundary, so the first page may be short ----
program:
        cmp     r7, #0
        beq     done
        ldr     r0, [r4, #B_UNIT]
        subs    r1, r0, #1
        and     r1, r6, r1              @ how far into its page this address is
        subs    r9, r0, r1              @ r9 = room left in the page
        cmp     r9, r7
        it      hi
        movhi   r9, r7                  @ r9 = the bytes this pass sends
        bl      write_enable
        cmp     r0, #0
        bne     finish
        bl      idle
        cmp     r0, #0
        bne     finish
        movs    r0, #FCR_CLEAR
        str     r0, [r5, #QSPI_FCR]
        subs    r0, r9, #1
        str     r0, [r5, #QSPI_DLR]     @ the length minus one
        ldr     r0, [r4, #B_CCR_OP]
        str     r0, [r5, #QSPI_CCR]
        str     r6, [r5, #QSPI_AR]      @ with data to send, the first DR write starts it
        mov     r10, r9                 @ r10 = bytes left to hand the FIFO
words:
        cmp     r10, #4
        blo     bytes
        movs    r1, #(FIFO_BYTES - 4)
        bl      room
        cmp     r0, #0
        bne     finish
        ldr     r0, [r8], #4            @ an unaligned source is fine: SRAM is normal memory
        str     r0, [r5, #QSPI_DR]
        subs    r10, r10, #4
        b       words
bytes:
        cmp     r10, #0
        beq     sent
        movs    r1, #(FIFO_BYTES - 1)
        bl      room
        cmp     r0, #0
        bne     finish
        ldrb    r0, [r8], #1
        strb    r0, [r5, #QSPI_DR]      @ a byte write adds one byte to the FIFO
        subs    r10, r10, #1
        b       bytes
sent:
        bl      complete
        cmp     r0, #0
        bne     finish
        bl      settle
        cmp     r0, #0
        bne     finish
        add     r6, r6, r9
        subs    r7, r7, r9
        b       program

done:
        movs    r0, #DONE
finish:
        str     r6, [r4, #B_AT]
        pop     {r4-r11, pc}

@ ---- helpers. r4 and r5 hold the block and the controller throughout. ----

@ idle: wait for SR.BUSY to clear. r0 = 0, or STUCK.
idle:
        ldr     r2, stuck_limit
1:      ldr     r0, [r5, #QSPI_SR]
        tst     r0, #SR_BUSY
        beq     2f
        subs    r2, r2, #1
        bne     1b
        movs    r0, #STUCK
        bx      lr
2:      movs    r0, #0
        bx      lr

@ room: wait until the FIFO holds at most r1 bytes. r0 = 0, or STUCK.
room:
        ldr     r2, stuck_limit
3:      ldr     r0, [r5, #QSPI_SR]
        ubfx    r0, r0, #8, #6          @ FLEVEL
        cmp     r0, r1
        bls     4f
        subs    r2, r2, #1
        bne     3b
        movs    r0, #STUCK
        bx      lr
4:      movs    r0, #0
        bx      lr

@ complete: wait for TCF and clear it, or report TEF. r0 = 0, TRANSFER_ERROR or STUCK.
complete:
        ldr     r2, stuck_limit
5:      ldr     r0, [r5, #QSPI_SR]
        tst     r0, #SR_TEF
        bne     7f
        tst     r0, #SR_TCF
        bne     6f
        subs    r2, r2, #1
        bne     5b
        movs    r0, #STUCK
        bx      lr
6:      movs    r0, #FCR_CTCF
        str     r0, [r5, #QSPI_FCR]
        movs    r0, #0
        bx      lr
7:      movs    r0, #FCR_CLEAR
        str     r0, [r5, #QSPI_FCR]
        movs    r0, #TRANSFER_ERROR
        bx      lr

@ status: read the status register once. r0 = 0 with the byte in r3, or an error code.
status:
        push    {lr}
        bl      idle
        cmp     r0, #0
        bne     8f
        movs    r0, #FCR_CLEAR
        str     r0, [r5, #QSPI_FCR]
        movs    r0, #0
        str     r0, [r5, #QSPI_DLR]     @ one byte
        ldr     r0, [r4, #B_CCR_RDSR]
        str     r0, [r5, #QSPI_CCR]     @ a read with no address: this write starts it
        bl      complete
        cmp     r0, #0
        bne     8f
        ldrb    r3, [r5, #QSPI_DR]
        str     r3, [r4, #B_STATUS]
        movs    r0, #0
8:      pop     {pc}

@ write_enable: send write enable, then require the write enable latch. r0 = 0 or a code.
write_enable:
        push    {lr}
        bl      idle
        cmp     r0, #0
        bne     9f
        movs    r0, #FCR_CLEAR
        str     r0, [r5, #QSPI_FCR]
        ldr     r0, [r4, #B_CCR_WREN]
        str     r0, [r5, #QSPI_CCR]     @ no address and no data: this write starts it
        bl      complete
        cmp     r0, #0
        bne     9f
        bl      status
        cmp     r0, #0
        bne     9f
        ldr     r0, [r4, #B_WEL]
        tst     r3, r0
        ite     eq
        moveq   r0, #NOT_ENABLED
        movne   r0, #0
9:      pop     {pc}

@ settle: read the status until write-in-progress clears, at most B_POLLS times.
settle:
        push    {r11, lr}
        ldr     r11, [r4, #B_POLLS]
10:     bl      status
        cmp     r0, #0
        bne     12f
        ldr     r0, [r4, #B_WIP]
        tst     r3, r0
        beq     11f
        subs    r11, r11, #1
        bne     10b
        movs    r0, #TIMEOUT
        b       12f
11:     movs    r0, #0
12:     pop     {r11, pc}

        .p2align 2
@ How long any one controller wait may spin before it is called stuck: a few million passes, far
@ beyond what one command of at most a page takes at any interface clock the controller offers.
stuck_limit:
        .word   0x00400000
