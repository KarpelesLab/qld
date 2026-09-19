// The destinations of `far.s`: absolute addresses 64 MiB from the image,
// one A32 (an even address) and one Thumb (bit 0 set).
	.global far_arm_qld
	.type far_arm_qld, %function
	.set far_arm_qld, 0x4000000

	.global far_thumb_qld
	.type far_thumb_qld, %function
	.set far_thumb_qld, 0x4000011
