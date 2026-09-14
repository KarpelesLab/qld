/* A Cortex-M style layout: a vector table at the start of flash, code and
   read-only data in flash, .data running in RAM but loaded from flash
   (AT>), and .bss in RAM. */
ENTRY(reset_handler)
MEMORY
{
  FLASH (rx)  : ORIGIN = 0x08000000, LENGTH = 64K
  RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 16K
}
_estack = ORIGIN(RAM) + LENGTH(RAM);
SECTIONS
{
  .isr_vector :
  {
    . = ALIGN(8);
    KEEP(*(.vectors))
    . = ALIGN(8);
  } >FLASH
  .text :
  {
    *(.text)
    *(.text*)
    . = ALIGN(8);
    _etext = .;
  } >FLASH =0x90909090
  .rodata :
  {
    *(.rodata) *(.rodata*)
    BYTE(0x11) SHORT(0x2233) LONG(0x44556677) QUAD(0x8899aabbccddeeff)
    . = ALIGN(8);
  } >FLASH
  .init_array :
  {
    PROVIDE_HIDDEN (__init_array_start = .);
    KEEP (*(SORT_BY_INIT_PRIORITY(.init_array.*)))
    PROVIDE_HIDDEN (__init_array_end = .);
  } >FLASH
  _sidata = LOADADDR(.data);
  .data :
  {
    . = ALIGN(8);
    _sdata = .;
    *(.data)
    *(.data*)
    . = ALIGN(8);
    _edata = .;
  } >RAM AT> FLASH
  .bss (NOLOAD) :
  {
    _sbss = .;
    *(.bss)
    *(.bss*)
    *(COMMON)
    . = ALIGN(8);
    _ebss = .;
  } >RAM
  ASSERT(SIZEOF(.data) + SIZEOF(.bss) <= LENGTH(RAM), "RAM overflow")
  /DISCARD/ : { *(.note.gnu.property) }
}
