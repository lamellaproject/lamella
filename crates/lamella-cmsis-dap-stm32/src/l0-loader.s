@ STM32L0 half-page flash programming loader, Thumb-1 (ARMv6-M, Cortex-M0+).
@
@ Programs WHOLE HALF-PAGES -- 16 words, 64 bytes -- per flash operation, where the single-word path
@ costs one operation and one host poll per word. RM0377 3.3.4 gives the half-page its own protocol,
@ and its Tprog is the SAME 3.2 ms as a single word's: sixteen times the bytes for the same wait.
@
@ WHY IT MUST RUN ON THE CORE, and this is a rule of the part rather than a performance choice.
@ RM0377: "When a half-page operation starts, the memory interface waits for 16 addresses/data,
@ aborting all read accesses that are not a fetch. A FETCH STOPS THE HALF-PAGE OPERATION" -- FWWERR,
@ and the memory unchanged. So the sixteen stores must arrive with no instruction fetch from NVM
@ between them, which a host driving one store per USB transaction cannot promise and code running
@ from RAM gets for free. `call_target` masks interrupts across the call for the same reason.
@
@ THE SIBLING LOADER IS NOT A DROP-IN AND THIS IS WHY. `f0-loader.s` programs HALF-WORDS one at a
@ time because an ST-Link cannot issue a 16-bit bus cycle; there the loader buys PORTABILITY. Here
@ the stores are ordinary words that any probe can issue, and the loader buys the BATCH -- a
@ different reason for the same shape.
@
@ On entry (the ABI `call_target` sets up):
@   r0 = source, in target RAM, word aligned
@   r1 = destination, in flash, HALF-PAGE aligned (the low 6 bits must be zero)
@   r2 = count of HALF-PAGES to program
@   r3 = FLASH peripheral base (0x40022000)
@ On exit:
@   r0 = 0 on success, else the FLASH_SR value whose error bits are set
@ The caller has already unlocked PECR and PRGLOCK and erased the pages spanned.

        .syntax unified
        .cpu    cortex-m0plus
        .thumb

        .equ    FLASH_PECR,  0x04
        .equ    FLASH_SR,    0x18
        .equ    SR_BSY,      0x01        @ FLASH_SR.BSY
        .equ    SR_EOP,      0x02        @ FLASH_SR.EOP, cleared by writing 1
        .equ    WORDS,       16          @ a half-page, RM0377 3.3.4

        .global _start
        .thumb_func
_start:
        @ PROG (bit 3) | FPRG (bit 10) = 0x408, set ONCE for the whole run. RM0377's own code
        @ example notes that for successive programming these may be hoisted out of the loop.
        @ 0x408 does not fit an ARMv6-M 8-bit immediate; 0x81 << 3 does.
        movs    r6, #0x81
        lsls    r6, r6, #3              @ r6 = 0x408 = PROG | FPRG
        ldr     r4, [r3, #FLASH_PECR]
        orrs    r4, r4, r6
        str     r4, [r3, #FLASH_PECR]

next_page:
        cmp     r2, #0
        beq     done

        @ ---- the sixteen stores, with NOTHING between them ----
        @ No branch out, no load from flash, no poll. The addresses stay inside the one half-page,
        @ which is all RM0377 requires of them: the memory interface increments internally, so it is
        @ the COUNT and the half-page that matter rather than the individual addresses.
        movs    r4, #WORDS
        mov     r7, r1                  @ walk a copy, so r1 still names the half-page after
store:
        ldr     r5, [r0]
        str     r5, [r7]
        adds    r0, r0, #4
        adds    r7, r7, #4
        subs    r4, r4, #1
        bne     store

wait:
        ldr     r5, [r3, #FLASH_SR]
        movs    r4, #SR_BSY
        tst     r5, r4
        bne     wait                    @ spin while BSY

        ldr     r4, errors
        tst     r5, r4
        bne     failed                  @ report the first half-page that went wrong, and stop

        movs    r4, #SR_EOP
        str     r4, [r3, #FLASH_SR]     @ EOP is cleared by writing it back as 1

        adds    r1, r1, #64             @ the next half-page
        subs    r2, r2, #1
        b       next_page

done:
        movs    r0, #0                  @ success
        b       finish

failed:
        mov     r0, r5                  @ report the offending FLASH_SR

finish:
        @ Clear PROG | FPRG whatever the outcome, so a failure does not leave the controller armed.
        movs    r6, #0x81
        lsls    r6, r6, #3
        ldr     r4, [r3, #FLASH_PECR]
        bics    r4, r4, r6
        str     r4, [r3, #FLASH_PECR]
        bx      lr                      @ back to the trap `call_target` planted

        .p2align 2
@ WRPERR(8) | PGAERR(9) | SIZERR(10) | OPTVERR(11) | RDERR(13) | NOTZEROERR(16) | FWWERR(17).
@ The same seven RM0377 3.5 names in one sentence, and the same set the driver's own mask carries --
@ a loader that watched fewer would return 0 for a half-page that failed.
errors:
        .word   0x00032F00
