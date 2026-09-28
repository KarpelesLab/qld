	section	.text,code
	xdef	helper
helper:
	move.l	#0,d0
	bsr.s	inner
	rts
inner:
	rts

	section	.data,data
	xdef	msg
msg:
	dc.b	"hello, amiga",0
	even
