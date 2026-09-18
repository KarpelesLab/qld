// A freestanding AArch64 program whose code has the Cortex-A53 erratum
// 843419 and 835769 sequences, linked with both workarounds: the linker
// moves the affected loads and the multiply-accumulate into patches that
// branch back. The program runs every patched instruction and adds up what
// they compute, so it prints and exits with the right code only if each
// patch ran its instruction (relocated) and returned to the right place.

	.text
	.global _start
	.type _start, %function
_start:
	mov	x20, #0
	bl	three_qld
	bl	four_qld
	bl	mac_qld

	// write(1, message, len)
	mov	x8, #64
	mov	x0, #1
	adrp	x1, message_qld
	add	x1, x1, :lo12:message_qld
	mov	x2, #message_len_qld
	svc	#0

	// exit(x20): 5 + 7 + 3 * 4 = 24
	mov	x8, #93
	mov	x0, x20
	svc	#0
	.size _start, . - _start

	// 843419, three instructions: the `adrp` at page offset 0xff8.
	.balign	4096
	.rept	1022
	nop
	.endr
	.type three_qld, %function
three_qld:
	adrp	x0, table_qld
	ldr	x1, [sp]
	ldr	x2, [x0, :lo12:table_qld]
	add	x20, x20, x2
	ret
	.size three_qld, . - three_qld

	// 843419, four instructions: the `adrp` at page offset 0xffc.
	.balign	4096
	.rept	1023
	nop
	.endr
	.type four_qld, %function
four_qld:
	adrp	x0, table_qld
	str	x1, [sp]
	add	x5, x5, #1
	ldr	x3, [x0, :lo12:table_qld + 8]
	add	x20, x20, x3
	ret
	.size four_qld, . - four_qld

	// 835769: a load, then a multiply-accumulate that does not use it.
	.type mac_qld, %function
mac_qld:
	mov	x4, #3
	mov	x5, #4
	adrp	x6, table_qld
	add	x6, x6, :lo12:table_qld
	ldr	x7, [x6]
	madd	x20, x4, x5, x20
	ret
	.size mac_qld, . - mac_qld

	.section .rodata, "a"
	.balign	8
table_qld:
	.xword	5, 7
message_qld:
	.ascii	"errata patches work\n"
	.set message_len_qld, . - message_qld
