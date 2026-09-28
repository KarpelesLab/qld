	section	.text,code
	xdef	_start
_start:
	rts
	dc.l	0,0

	section	.data,data
	xdef	dz
dz:
	dc.l	$11223344
	dc.l	0
	dc.b	1,0,0
