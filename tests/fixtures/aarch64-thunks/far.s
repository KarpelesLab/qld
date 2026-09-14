// A freestanding AArch64 program with a call and a jump to symbols 512 MiB
// away, which no `bl`/`b` can reach: the linker has to insert
// range-extension thunks. The far branches are never taken at run time (the
// guard is always false), so the program prints and exits normally while
// still forcing the thunks to be generated and relocated.

	.text
	.global _start
	.type _start, %function
_start:
	// write(1, message, len)
	mov	x8, #64
	mov	x0, #1
	adrp	x1, message_qld
	add	x1, x1, :lo12:message_qld
	mov	x2, #message_len_qld
	svc	#0

	// A guard the linker cannot fold away: the byte in .data is zero.
	adrp	x3, guard_qld
	ldrb	w4, [x3, :lo12:guard_qld]
	cbz	w4, 2f
	bl	far_function_qld
	b	far_tail_qld
2:
	// exit(7)
	mov	x8, #93
	mov	x0, #7
	svc	#0
	.size _start, . - _start

	// Two more callers of the same destination: they must share the
	// thunk the first one needed.
	.global unused_caller_qld
	.type unused_caller_qld, %function
unused_caller_qld:
	bl	far_function_qld
	bl	far_function_qld
	ret
	.size unused_caller_qld, . - unused_caller_qld

	.global far_function_qld
	.set far_function_qld, 0x20000000
	.global far_tail_qld
	.set far_tail_qld, 0x20000010

	.section .rodata, "a"
message_qld:
	.ascii	"thunks work\n"
	.set message_len_qld, . - message_qld

	.data
guard_qld:
	.byte	0
