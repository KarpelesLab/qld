# A freestanding PowerPC64 LE program with a call and a jump to symbols
# 256 MiB away, which no `bl`/`b` can reach: the linker has to insert
# range-extension thunks. The far branches are never taken at run time (the
# guard is always false), so the program prints and exits normally while
# still forcing the thunks to be generated and relocated.

	.abiversion 2
	.text
	.globl	_start
	.type	_start, @function
_start:
	# The kernel does not set r12 at the entry point: find the TOC from
	# the program counter.
	bcl	20, 31, 1f
1:	mflr	2
	addis	2, 2, .TOC.-1b@ha
	addi	2, 2, .TOC.-1b@l

	# write(1, message, len)
	li	0, 4
	li	3, 1
	addis	4, 2, message_qld@toc@ha
	addi	4, 4, message_qld@toc@l
	li	5, message_len_qld
	sc

	# A guard the linker cannot fold away: the byte in .data is zero.
	addis	6, 2, guard_qld@toc@ha
	lbz	6, guard_qld@toc@l(6)
	cmpwi	6, 0
	beq	2f
	bl	far_function_qld
	nop
	b	far_tail_qld
2:
	# exit(7)
	li	0, 1
	li	3, 7
	sc
	.size	_start, . - _start

	# Two more callers of the same destination: they share the thunk the
	# first one needed.
	.globl	unused_caller_qld
	.type	unused_caller_qld, @function
unused_caller_qld:
	bl	far_function_qld
	nop
	bl	far_function_qld
	nop
	blr
	.size	unused_caller_qld, . - unused_caller_qld

	.globl	far_function_qld
	.set	far_function_qld, 0x20000000
	.globl	far_tail_qld
	.set	far_tail_qld, 0x20000010

	.section .rodata, "a"
message_qld:
	.ascii	"thunks work\n"
	.set	message_len_qld, . - message_qld

	.data
guard_qld:
	.byte	0
