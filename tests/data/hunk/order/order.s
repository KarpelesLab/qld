	section	.zdata,data
	xdef	zsym
zsym:
	dc.l	$aabbccdd

	section	.bss,bss
	xdef	bsym
bsym:
	ds.l	1

	section	.text,code
	xdef	_start
_start:
	move.l	zsym,d0
	rts
