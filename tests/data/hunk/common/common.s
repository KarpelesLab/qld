CUSTOM	equ	$dff180
	xdef	hwaddr

	section	.text,code
	xdef	_start
_start:
	move.w	#$0f00,CUSTOM
	rts

	section	.data,data
hwaddr:
	dc.l	CUSTOM
	dc.l	buffer

	comm	buffer,64
