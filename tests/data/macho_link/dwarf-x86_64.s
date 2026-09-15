# call_through(fn): calls fn with a frame whose CFI compact unwind cannot
# express (the DW_CFA_nop escape), so the assembler emits a DWARF-mode
# compact unwind record and the unwinder needs the FDE in __eh_frame.
    .text
    .globl _call_through
    .p2align 4
_call_through:
    .cfi_startproc
    pushq %rbp
    .cfi_def_cfa_offset 16
    .cfi_offset %rbp, -16
    movq %rsp, %rbp
    .cfi_def_cfa_register %rbp
    .cfi_escape 0x00
    callq *%rdi
    popq %rbp
    retq
    .cfi_endproc

.subsections_via_symbols
