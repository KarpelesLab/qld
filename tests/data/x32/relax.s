# The GOTPCRELX forms GNU ld relaxes for x32, freestanding: loads become
# `lea`, indirect calls and jumps direct ones, and (in position-dependent
# output) `test` and binary operators take the address as an immediate,
# with or without a REX prefix. `ext` is an undefined weak symbol, which
# resolves to zero in a static executable.
	.text
	.globl	_start
	.type	_start, @function
_start:
	movl	data@GOTPCREL(%rip), %eax
	movl	data@GOTPCREL(%rip), %r9d
	movq	data@GOTPCREL(%rip), %rax
	movq	data@GOTPCREL(%rip), %r11
	call	*func@GOTPCREL(%rip)
	addl	data@GOTPCREL(%rip), %eax
	addl	data@GOTPCREL(%rip), %r10d
	addq	data@GOTPCREL(%rip), %rax
	addq	data@GOTPCREL(%rip), %r12
	adcl	data@GOTPCREL(%rip), %ecx
	andl	data@GOTPCREL(%rip), %edx
	cmpl	data@GOTPCREL(%rip), %esi
	orl	data@GOTPCREL(%rip), %edi
	sbbl	data@GOTPCREL(%rip), %ebp
	subl	data@GOTPCREL(%rip), %esp
	xorq	data@GOTPCREL(%rip), %r15
	testl	%eax, data@GOTPCREL(%rip)
	testl	%r13d, data@GOTPCREL(%rip)
	testq	%rbx, data@GOTPCREL(%rip)
	movl	ext@GOTPCREL(%rip), %eax
	addl	ext@GOTPCREL(%rip), %eax
	movl	func@GOTPCREL(%rip), %ecx
	# The address itself, as a word the dynamic linker relocates in PIC.
	movl	ptr(%rip), %eax
	jmp	*func@GOTPCREL(%rip)
	.size	_start, .-_start

	.globl	func
	.type	func, @function
func:
	ret
	.size	func, .-func

	.weak	ext

	.data
	.align	4
	.globl	data
	.type	data, @object
data:	.long	1
	.size	data, 4
ptr:	.long	data
	.long	func
