	section	.text,code
	xdef	_start
_start:
	move.l	#message,d0
	jsr	helper
	move.l	counter,d1
	lea	table(pc),a0
	rts

	section	.data,data
	xdef	table
table:
	dc.l	_start
	dc.l	message
	dc.l	helper

	section	.bss,bss
	xdef	counter
counter:
	ds.l	4
