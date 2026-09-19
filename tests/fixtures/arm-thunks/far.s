// A freestanding 32-bit Arm program with calls and jumps to symbols 64 MiB
// away, which no `bl`, `blx` or `b.w` can reach: the linker has to insert
// range-extension thunks, in the caller's instruction set, and the A32
// callers of the Thumb destination also need the state change. The far
// branches are never taken at run time (the guard is always false), so the
// program prints and exits normally while still forcing the thunks to be
// generated and relocated.

	.syntax unified
	.text
	.arm
	.global _start
	.type _start, %function
_start:
	// write(1, message, len)
	mov	r7, #4
	mov	r0, #1
	ldr	r1, =message_qld
	mov	r2, #12			// strlen("thunks work\n")
	svc	#0

	// A guard the linker cannot fold away: the byte in .data is zero.
	ldr	r3, =guard_qld
	ldrb	r4, [r3]
	cmp	r4, #0
	beq	2f
	bl	far_arm_qld
	bl	far_thumb_qld
	b	far_arm_qld
2:
	// exit(7)
	mov	r7, #1
	mov	r0, #7
	svc	#0
	.size _start, . - _start

	// Two more callers of the same destination: they must share the
	// thunk the first one needed.
	.global unused_caller_qld
	.type unused_caller_qld, %function
unused_caller_qld:
	bl	far_arm_qld
	bl	far_arm_qld
	bx	lr
	.size unused_caller_qld, . - unused_caller_qld

	.thumb
	.global unused_thumb_caller_qld
	.thumb_func
	.type unused_thumb_caller_qld, %function
unused_thumb_caller_qld:
	bl	far_thumb_qld
	bl	far_arm_qld
	b.w	far_thumb_qld
	.size unused_thumb_caller_qld, . - unused_thumb_caller_qld

	.section .rodata, "a"
message_qld:
	.ascii	"thunks work\n"

	.data
guard_qld:
	.byte	0
