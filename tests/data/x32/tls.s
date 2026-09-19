# The TLS access sequences of the x32 psABI (GNU dialect and descriptors)
# in every assembler form GNU ld relaxes, freestanding. Linked into a
# static executable every one relaxes to local-exec; into a shared object
# they stay dynamic; with `EXTERN` the variables are another module's, so
# a PIE relaxes general-dynamic and descriptors to initial-exec.
	.text
	.globl	_start
	.type	_start, @function
_start:
	# general-dynamic, direct and indirect call
	leaq	gd@tlsgd(%rip), %rdi
	.word	0x6666
	rex64
	call	__tls_get_addr@PLT
	leaq	gd@tlsgd(%rip), %rdi
	.byte	0x66
	rex64
	call	*__tls_get_addr@GOTPCREL(%rip)
	.ifndef EXTERN
	# local-dynamic, direct call
	leaq	ld@tlsld(%rip), %rdi
	call	__tls_get_addr@PLT
	movl	ld@dtpoff(%rax), %ecx
	.ifdef LD_INDIRECT
	# and indirect: GNU ld 2.46 writes the sequence of the direct form
	# over it and leaves a byte of the call behind (docs/compatibility.md)
	leaq	ld@tlsld(%rip), %rdi
	call	*__tls_get_addr@GOTPCREL(%rip)
	leal	ld@dtpoff(%rax), %edx
	.endif
	.endif
	# initial-exec: without REX, with an empty REX, with REX.R and REX.W
	movl	%fs:0, %eax
	addl	ie@gottpoff(%rip), %eax
	rex addl ie@gottpoff(%rip), %eax
	addl	ie@gottpoff(%rip), %r9d
	addl	ie@gottpoff(%rip), %esp
	addl	ie@gottpoff(%rip), %r12d
	movl	ie@gottpoff(%rip), %ecx
	movl	ie@gottpoff(%rip), %r8d
	movq	ie@gottpoff(%rip), %rax
	movq	ie@gottpoff(%rip), %r10
	addq	ie@gottpoff(%rip), %r11
	addq	ie@gottpoff(%rip), %rsp
	# descriptors, x32 (`rex leal`, `call *(%eax)`) and LP64 forms
	leal	desc@tlsdesc(%rip), %eax
	call	*desc@tlscall(%eax)
	leal	desc@tlsdesc(%rip), %r9d
	leaq	desc@tlsdesc(%rip), %rax
	call	*desc@tlscall(%rax)
	.ifndef PIC
	.ifndef EXTERN
	# local-exec (executables only)
	movl	%fs:le@tpoff, %eax
	movl	%fs:0, %eax
	leal	le@tpoff(%rax), %eax
	.endif
	.endif
	ret
	.size	_start, .-_start

	.globl	__tls_get_addr
	.type	__tls_get_addr, @function
__tls_get_addr:
	ret
	.size	__tls_get_addr, .-__tls_get_addr

	.ifndef EXTERN
	.section .tdata,"awT",@progbits
	.align	4
	.globl	gd
gd:	.long	1
ld:	.long	2
	.globl	ie
ie:	.long	3
	.globl	le
le:	.long	4
	.globl	desc
desc:	.long	5
	.endif
