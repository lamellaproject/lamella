@ STM32 F0/F1/L0 flash programming loader, Thumb-1 (ARMv6-M, Cortex-M0).
@
@ Programs half-words from RAM into flash. It exists because these controllers program 16 bits at a
@ time and a probe cannot be relied on to issue a genuine 16-bit BUS cycle -- an ST-Link has no
@ 16-bit access at all. Running the stores on the CORE makes the access width the core's business,
@ so this works identically through any probe.
@
@ On entry (the ABI `call_target` sets up):
@   r0 = source, in target RAM, half-word aligned
@   r1 = destination, in flash, half-word aligned
@   r2 = count of HALF-WORDS to program
@   r3 = FLASH peripheral base (0x40022000)
@ On exit:
@   r0 = 0 on success, else the FLASH_SR value whose error bits are set
@ The caller has already unlocked FLASH_CR and erased the target pages.

        .syntax unified
        .cpu    cortex-m0
        .thumb

        .equ    FLASH_SR, 0x0C
        .equ    FLASH_CR, 0x10
        .equ    CR_PG,    0x01          @ FLASH_CR.PG: program enable
        .equ    SR_BSY,   0x01          @ FLASH_SR.BSY
        .equ    SR_ERRS,  0x14          @ FLASH_SR: PGERR (bit 2) | WRPRTERR (bit 4)

        .global _start
_start:
        ldr     r4, [r3, #FLASH_CR]
        movs    r5, #CR_PG
        orrs    r4, r4, r5
        str     r4, [r3, #FLASH_CR]     @ PG = 1 for the whole run

next:
        cmp     r2, #0
        beq     done

        ldrh    r4, [r0]                @ the 16-bit store IS the program operation
        strh    r4, [r1]

wait:
        ldr     r5, [r3, #FLASH_SR]
        movs    r6, #SR_BSY
        tst     r5, r6
        bne     wait                    @ spin while BSY

        movs    r6, #SR_ERRS
        tst     r5, r6
        bne     failed                  @ PGERR or WRPRTERR -> stop and report

        adds    r0, r0, #2
        adds    r1, r1, #2
        subs    r2, r2, #1
        b       next

done:
        movs    r0, #0                  @ success
        b       finish

failed:
        movs    r0, r5                  @ report the offending FLASH_SR

finish:
        ldr     r4, [r3, #FLASH_CR]
        movs    r5, #CR_PG
        bics    r4, r4, r5
        str     r4, [r3, #FLASH_CR]     @ PG = 0, whatever the outcome
        bx      lr                      @ back to the trap `call_target` planted

        .p2align 2              @ whole words, so the host can stage it with a word-wise write
