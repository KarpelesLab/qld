# The TLS access sequences of the i386 psABI (GNU dialect and descriptors)
# in both of their assembler forms, freestanding. Linked into a static
# executable every one relaxes to local-exec; into a shared object they
# stay dynamic; `EXTERN` makes `ext` another module's, for initial-exec.
	.text
	.globl	_start
	.type	_start, @function
_start:
	call	1f
1:	popl	%ebx
	addl	$_GLOBAL_OFFSET_TABLE_+(.-1b), %ebx
	# general-dynamic, SIB form with a direct call
	leal	gd@tlsgd(,%ebx,1), %eax
	call	___tls_get_addr@PLT
	# general-dynamic, base register with an indirect call
	leal	gd@tlsgd(%ebx), %eax
	call	*___tls_get_addr@GOT(%ebx)
	# local-dynamic, direct and indirect call
	leal	ld@tlsldm(%ebx), %eax
	call	___tls_get_addr@PLT
	movl	ld@dtpoff(%eax), %ecx
	leal	ld@tlsldm(%ebx), %eax
	call	*___tls_get_addr@GOT(%ebx)
	leal	ld@dtpoff(%eax), %edx
	# initial-exec, GOT-relative
	movl	ie@gotntpoff(%ebx), %ecx
	movl	%gs:(%ecx), %eax
	addl	ie@gotntpoff(%ebx), %edx
	# descriptors
	leal	desc@tlsdesc(%ebx), %eax
	call	*desc@tlscall(%eax)
	.ifdef ABSOLUTE
	# local-exec (executables only)
	movl	%gs:0, %eax
	leal	le@ntpoff(%eax), %eax
	# initial-exec, absolute GOT address (position-dependent only)
	movl	ie@indntpoff, %eax
	movl	ie@indntpoff, %ecx
	addl	ie@indntpoff, %edx
	.endif
	ret
	.size	_start, .-_start

	.globl	___tls_get_addr
	.type	___tls_get_addr, @function
___tls_get_addr:
	ret
	.size	___tls_get_addr, .-___tls_get_addr

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
