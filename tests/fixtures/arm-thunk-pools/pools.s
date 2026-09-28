// A 32-bit Arm program whose `.text` is larger than a Thumb `bl` reaches
// (+/-16 MiB): the caller at the start cannot reach a thunk pool at the
// end of the section, so the linker has to put one in the middle.
//
// The padding sections are where a pool can go; the call at the start is
// really taken at run time, so the thunk it goes through has to work.

	.syntax unified
	.section .text.start, "ax", %progbits
	.thumb
	.global _start
	.thumb_func
	.type _start, %function
_start:
	// far_fn_qld returns 7; it is 17 MiB away, so the `bl` goes through
	// a thunk in the pool nearest this caller.
	bl	far_fn_qld
	mov	r4, r0

	// write(1, message, len)
	mov	r7, #4
	mov	r0, #1
	ldr	r1, =message_qld
	mov	r2, #11			// strlen("pools work\n")
	svc	#0

	// exit(r4)
	mov	r7, #1
	mov	r0, r4
	svc	#0
	.size _start, . - _start

	.section .rodata, "a"
message_qld:
	.ascii	"pools work\n"

	.section .text.pad00, "ax", %progbits
	.space 0x100000

	.section .text.pad01, "ax", %progbits
	.space 0x100000

	.section .text.pad02, "ax", %progbits
	.space 0x100000

	.section .text.pad03, "ax", %progbits
	.space 0x100000

	.section .text.pad04, "ax", %progbits
	.space 0x100000

	.section .text.pad05, "ax", %progbits
	.space 0x100000

	.section .text.pad06, "ax", %progbits
	.space 0x100000

	.section .text.pad07, "ax", %progbits
	.space 0x100000

	.section .text.pad08, "ax", %progbits
	.space 0x100000

	.section .text.pad09, "ax", %progbits
	.space 0x100000

	.section .text.pad10, "ax", %progbits
	.space 0x100000

	.section .text.pad11, "ax", %progbits
	.space 0x100000

	.section .text.pad12, "ax", %progbits
	.space 0x100000

	.section .text.pad13, "ax", %progbits
	.space 0x100000

	.section .text.pad14, "ax", %progbits
	.space 0x100000

	.section .text.pad15, "ax", %progbits
	.space 0x100000

	.section .text.pad16, "ax", %progbits
	.space 0x100000

	.section .text.far, "ax", %progbits
	.thumb
	.global far_fn_qld
	.thumb_func
	.type far_fn_qld, %function
far_fn_qld:
	movs	r0, #7
	bx	lr
	.size far_fn_qld, . - far_fn_qld
