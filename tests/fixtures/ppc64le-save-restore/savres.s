# A freestanding PowerPC64 LE program that saves and restores its
# nonvolatile registers through the out-of-line routines the linker
# provides (`_savegpr0_*`, `_restgpr0_*`, `_savefpr_*`, `_restfpr_*`,
# `_savevr_*`, `_restvr_*`), as GCC's `-Os` code does: nothing on the
# command line defines them. Each worker clobbers the registers its
# routines saved; `_start` checks they came back.

	.abiversion 2
	.text
	.globl	_start
	.type	_start, @function
_start:
	bcl	20, 31, 1f
1:	mflr	2
	addis	2, 2, .TOC.-1b@ha
	addi	2, 2, .TOC.-1b@l
	stdu	1, -64(1)

	# General-purpose registers, restored with the link register.
	li	14, 1414
	li	31, 3131
	bl	work_gpr
	cmpdi	14, 1414
	bne	fail
	cmpdi	31, 3131
	bne	fail

	# Floating-point registers.
	addis	9, 2, values@toc@ha
	addi	9, 9, values@toc@l
	lfd	14, 0(9)
	lfd	31, 8(9)
	bl	work_fpr
	addis	9, 2, values@toc@ha
	addi	9, 9, values@toc@l
	stfd	14, 32(1)
	stfd	31, 40(1)
	ld	10, 32(1)
	ld	11, 0(9)
	cmpd	10, 11
	bne	fail
	ld	10, 40(1)
	ld	11, 8(9)
	cmpd	10, 11
	bne	fail

	# Vector registers.
	vspltisw 20, 5
	vspltisw 31, -3
	bl	work_vr
	vspltisw 0, 5
	vcmpequw. 0, 20, 0
	bge	6, fail
	vspltisw 0, -3
	vcmpequw. 0, 31, 0
	bge	6, fail

	# write(1, message, len); exit(0)
	li	0, 4
	li	3, 1
	addis	4, 2, message@toc@ha
	addi	4, 4, message@toc@l
	li	5, message_len
	sc
	li	0, 1
	li	3, 0
	sc
fail:
	li	0, 1
	li	3, 1
	sc
	.size	_start, . - _start

	.type	work_gpr, @function
work_gpr:
	mflr	0
	bl	_savegpr0_14
	stdu	1, -176(1)
	li	14, 0
	li	20, 0
	li	31, 0
	addi	1, 1, 176
	b	_restgpr0_14
	.size	work_gpr, . - work_gpr

	.type	work_fpr, @function
work_fpr:
	mflr	0
	bl	_savefpr_14
	stdu	1, -176(1)
	fsub	14, 14, 14
	fsub	31, 31, 31
	addi	1, 1, 176
	b	_restfpr_14
	.size	work_fpr, . - work_fpr

	# r0 points past the 192-byte save area of v20-v31.
	.type	work_vr, @function
work_vr:
	mflr	0
	std	0, 16(1)
	stdu	1, -256(1)
	addi	0, 1, 224
	bl	_savevr_20
	vxor	20, 20, 20
	vxor	31, 31, 31
	addi	0, 1, 224
	bl	_restvr_20
	addi	1, 1, 256
	ld	0, 16(1)
	mtlr	0
	blr
	.size	work_vr, . - work_vr

	.section .rodata, "a"
	.p2align 3
values:
	.double	1.5
	.double	-2.25
message:
	.ascii	"save/restore works\n"
	.set	message_len, . - message
