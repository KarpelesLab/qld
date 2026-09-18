# A freestanding i386 program that takes an access violation under an SEH
# handler that resumes execution, then exits with 42.
#
# The object is SafeSEH-compatible (bit 0 of `@feat.00`) and carries its own
# load configuration, which refers to the handler table the linker builds.
# Built with `--defsym UNREGISTERED=1`, it registers a decoy instead of the
# handler: Windows must then refuse to call the handler, and the process
# dies of the access violation.
	.def	@feat.00; .scl 3; .type 0; .endef
	.globl	@feat.00
.set @feat.00, 1

	.text
	.globl	_start
	.def	_start; .scl 2; .type 32; .endef
_start:
	pushl	$_handler
	pushl	%fs:0
	movl	%esp, %fs:0
	xorl	%eax, %eax
	movl	(%eax), %eax
_resume:
	movl	(%esp), %eax
	movl	%eax, %fs:0
	addl	$8, %esp
	pushl	$42
	calll	*__imp__ExitProcess@4

# EXCEPTION_DISPOSITION handler(EXCEPTION_RECORD *, void *frame,
#                               CONTEXT *, void *dispatcher)
	.def	_handler; .scl 3; .type 32; .endef
_handler:
	movl	12(%esp), %eax
	movl	$_resume, 0xb8(%eax)	# CONTEXT.Eip
	xorl	%eax, %eax		# ExceptionContinueExecution
	retl

	.def	_decoy; .scl 3; .type 32; .endef
_decoy:
	movl	$1, %eax		# ExceptionContinueSearch
	retl

.ifdef UNREGISTERED
	.safeseh _decoy
.else
	.safeseh _handler
.endif

# IMAGE_LOAD_CONFIG_DIRECTORY32 up to SEHandlerCount.
	.section .rdata,"dr"
	.globl	__load_config_used
	.p2align 2
__load_config_used:
	.long	72
	.fill	60, 1, 0
	.long	___safe_se_handler_table
	.long	___safe_se_handler_count
