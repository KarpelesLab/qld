// The destinations of the branches in arm64-near.s. The padding object
// linked between the two files puts them out of reach of `tbnz`
// (±32 KiB) and `cbz` (±1 MiB), so those branches need thunks.
	.text
	.globl	far_odd
	.p2align 2
far_odd:
	mov	w0, #7
	ret

	.globl	far_zero
	.p2align 2
far_zero:
	mov	w0, #5
	ret

	.globl	far_even
	.p2align 2
far_even:
	mov	w0, #9
	ret

// A function with a frame, whose unwind information the assembler packs
// into its `.pdata` entry.
	.globl	far_function
	.p2align 2
	.seh_proc far_function
far_function:
	stp	x29, x30, [sp, #-16]!
	.seh_save_fplr_x 16
	mov	x29, sp
	.seh_set_fp
	.seh_endprologue
	add	w0, w0, #1
	.seh_startepilogue
	ldp	x29, x30, [sp], #16
	.seh_save_fplr_x 16
	.seh_endepilogue
	ret
	.seh_endproc

// The entry point when this file is linked as a DLL on its own.
	.globl	DllMainCRTStartup
	.p2align 2
DllMainCRTStartup:
	mov	w0, #1
	ret
