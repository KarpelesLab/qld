/* A bare-metal AArch64 image: read-only sections in flash at 0x40000000,
   writable sections in RAM at 0x80000000 with their load address in flash,
   as firmware is laid out. */
ENTRY(_reset)

MEMORY
{
  FLASH (rx) : ORIGIN = 0x40000000, LENGTH = 1M
  RAM   (rwx): ORIGIN = 0x80000000, LENGTH = 1M
}

SECTIONS
{
  .vectors : { KEEP(*(.vectors)) } > FLASH
  .text : { *(.text .text.*) } > FLASH
  .rodata : { *(.rodata .rodata.*) } > FLASH
  _sidata = LOADADDR(.data);
  .data : { _sdata = .; *(.data .data.*) . = ALIGN(8); _edata = .; } > RAM AT> FLASH
  .bss (NOLOAD) : { _sbss = .; *(.bss .bss.*) *(COMMON) . = ALIGN(8); _ebss = .; } > RAM
  _stack_top = ORIGIN(RAM) + LENGTH(RAM);
  /DISCARD/ : { *(.comment) *(.note.*) }
}
