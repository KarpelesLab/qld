// call_through(fn): calls fn with a frame whose CFI compact unwind cannot
// express (the DW_CFA_nop escape), so the assembler emits a DWARF-mode
// compact unwind record and the unwinder needs the FDE in __eh_frame.
    .text
    .globl _call_through
    .p2align 2
_call_through:
    .cfi_startproc
    stp x29, x30, [sp, #-16]!
    .cfi_def_cfa_offset 16
    .cfi_offset w30, -8
    .cfi_offset w29, -16
    mov x29, sp
    .cfi_def_cfa w29, 16
    .cfi_escape 0x00
    blr x0
    ldp x29, x30, [sp], #16
    ret
    .cfi_endproc

.subsections_via_symbols
