	section	.text,code
	xdef	_start
_start:
	move.l	#msg,d0
	jsr	helper
	move.l	counter,d1
	lea	tab2(pc),a0
	rts
tab2:
	dc.w	1,2

	section	.rodata,data
	xdef	ro
ro:
	dc.l	$12345678
	dc.b	"ro",0
	even

	section	.data,data
	xdef	tab
tab:
	dc.l	_start
	dc.l	0
	dc.l	0

	section	.bss,bss
	xdef	counter
counter:
	ds.l	4
