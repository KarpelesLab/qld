// Conditional tail calls to functions in arm64-far.s: `tbnz` (BRANCH14)
// and `cbz` (BRANCH19) cannot reach them across the padding, `b`
// (BRANCH26) can.
	.text
	.globl	near_dispatch
	.p2align 2
near_dispatch:
	tbnz	w0, #0, far_odd
	cbz	w0, far_zero
	b	far_even
