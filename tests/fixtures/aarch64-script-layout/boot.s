// A bare-metal AArch64 image: a vector entry, code that reaches its data
// through `adrp`/`add` and `adrp`/`ldr`, and a table the script copies from
// flash to RAM.

	.section .vectors, "ax", %progbits
	.global _reset
_reset:
	b	_start

	.text
	.global _start
	.type _start, %function
_start:
	adrp	x0, value_qld
	ldr	w1, [x0, :lo12:value_qld]
	adrp	x2, table_qld
	add	x2, x2, :lo12:table_qld
	ldr	x3, [x2]
	adrp	x4, bss_word_qld
	str	w1, [x4, :lo12:bss_word_qld]
	bl	helper_qld
	b	.
	.size _start, . - _start

	.global helper_qld
	.type helper_qld, %function
helper_qld:
	ret
	.size helper_qld, . - helper_qld

	.section .rodata, "a"
	.align 3
table_qld:
	.xword	_start
	.xword	helper_qld

	.data
	.align 2
	.global value_qld
value_qld:
	.word	0x1234

	.bss
	.align 2
bss_word_qld:
	.zero	4
