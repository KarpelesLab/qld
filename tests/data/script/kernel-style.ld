/* A Linux-kernel-style x86-64 vmlinux script, written for qld's tests in
   the shape of arch/x86/kernel/vmlinux.lds.S after preprocessing. */
# 1 "arch/x86/kernel/vmlinux.lds.S"
OUTPUT_FORMAT("elf64-x86-64")
OUTPUT_ARCH(i386:x86-64)
ENTRY(phys_startup_64)

jiffies = jiffies_64;
__per_cpu_load = 0;

PHDRS {
 text PT_LOAD FLAGS(5);
 data PT_LOAD FLAGS(6);
 percpu PT_LOAD FLAGS(6);
 init PT_LOAD FLAGS(7);
 note PT_NOTE FLAGS(0);
}

SECTIONS
{
 . = __START_KERNEL;
 phys_startup_64 = ABSOLUTE(startup_64 - 0xffffffff80000000);

 .text : AT(ADDR(.text) - 0xffffffff80000000) {
  _text = .;
  _stext = .;
  . = ALIGN(0x1000);
  KEEP(*(.head.text))
  . = ALIGN(8); __noinstr_text_start = .; *(.noinstr.text) __noinstr_text_end = .;
  *(.text.hot .text.hot.*) *(.text .text.fixup) *(.text.unlikely .text.unlikely.*)
  . = ALIGN((1 << 12)); __entry_text_start = .; *(.entry.text) __entry_text_end = .;
  *(.gnu.warning)
 } :text = 0xcccccccc

 . = ALIGN(0x200000);
 _etext = .;

 .rodata : AT(ADDR(.rodata) - 0xffffffff80000000) {
  __start_rodata = .;
  *(.rodata) *(.rodata.*)
  . = ALIGN(8);
  __start___ksymtab = .;
  KEEP(*(SORT(___ksymtab+*)))
  __stop___ksymtab = .;
 } :data

 .notes : AT(ADDR(.notes) - 0xffffffff80000000) {
  __start_notes = .; KEEP(*(.note.*)) __stop_notes = .;
 } :data :note

 .data : AT(ADDR(.data) - 0xffffffff80000000) {
  . = ALIGN((1 << 12) << (2 + 0));
  __start_init_task = .;
  init_thread_union = .;
  init_stack = .;
  KEEP(*(.data..init_task))
  . = __start_init_task + ((1 << 12) << (2 + 0));
  __end_init_task = .;
  *(.data..cacheline_aligned)
  _edata = .;
 } :data

 PROVIDE(__ref_stack_chk_guard = __stack_chk_guard);

 .data..percpu 0 : AT(ADDR(.data..percpu) + __per_cpu_load - 0xffffffff80000000) {
  __per_cpu_start = .;
  *(.data..percpu..first)
  . = ALIGN((1 << 12));
  *(.data..percpu..page_aligned)
  . = ALIGN(64);
  *(.data..percpu..read_mostly)
  *(.data..percpu)
  __per_cpu_end = .;
 } :percpu

 .init.data : AT(ADDR(.init.data) - 0xffffffff80000000) {
  KEEP(*(SORT(.init_array.*))) KEEP(*(.init_array .ctors))
  __initcall_start = .; KEEP(*(.initcallearly.init))
  __initcall0_start = .; KEEP(*(.initcall0.init)) KEEP(*(.initcall0s.init))
 } :init

 .bss : AT(ADDR(.bss) - 0xffffffff80000000) {
  __bss_start = .;
  *(.bss..page_aligned)
  . = ALIGN(PAGE_SIZE);
  *(BSS_MAIN)
  *(.bss..decrypted)
  . = ALIGN(PAGE_SIZE);
  __bss_stop = .;
 }

 . = ALIGN(0x1000);
 _end = .;

 .stab 0 : { *(.stab) }
 .comment 0 : { *(.comment) }
 .debug_info 0 : { *(.debug_info .gnu.linkonce.wi.*) }

 /DISCARD/ : {
  *(.eh_frame)
  *(.discard) *(.discard.*)
  *(.exitcall.exit)
 }
}

. = ASSERT((_end - _text <= (512 * 1024 * 1024)), "kernel image bigger than KERNEL_IMAGE_SIZE");
ASSERT(__per_cpu_end - __per_cpu_start == SIZEOF(.data..percpu), "per-CPU size mismatch")
