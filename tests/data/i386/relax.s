# Every R_386_GOT32X form GNU ld relaxes, plus the GOT-relative relocations
# around them, in one freestanding function. Linked position-dependent and
# as a PIE; `BASELESS` adds the forms without a base register, which only
# position-dependent output may use.
	.text
	.globl	_start
	.type	_start, @function
_start:
	call	1f
1:	popl	%ebx
	addl	$_GLOBAL_OFFSET_TABLE_+(.-1b), %ebx
	movl	data@GOT(%ebx), %eax
	cmpl	data@GOT(%ebx), %esi
	addl	data@GOT(%ebx), %edi
	testl	%edx, data@GOT(%ebx)
	call	*func@GOT(%ebx)
	leal	data@GOTOFF(%ebx), %edx
	movl	other@GOT(%ebx), %ecx
	movl	weakref@GOT(%ebx), %eax
	.ifdef BASELESS
	movl	data@GOT, %ecx
	.endif
	movl	$1, %eax
	jmp	*func@GOT(%ebx)
	.size	_start, .-_start

	.globl	func
	.type	func, @function
func:
	ret
	.size	func, .-func

	.data
	.globl	data
	.type	data, @object
data:	.long	1, 2
	.size	data, 8
	.globl	other
other:	.long	data
	.weak	weakref
