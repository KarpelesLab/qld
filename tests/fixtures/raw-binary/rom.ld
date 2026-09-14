/* ROM at 0x1000, RAM at 0x8000. A flat binary holds the load image from
   the lowest load address: gaps between sections are zero bytes, gaps
   inside a section use its fill pattern, and .bss is not stored. */
ENTRY(_start)
MEMORY
{
  ROM (rx) : ORIGIN = 0x1000, LENGTH = 256
  RAM (rw) : ORIGIN = 0x8000, LENGTH = 256
}
SECTIONS
{
  .text : { *(.text) . = ALIGN(16); } >ROM =0xcc
  .rodata : { *(.rodata) LONG(ADDR(.data)) LONG(LOADADDR(.data)) } >ROM
  .data : { *(.data) SHORT(0xbeef) } >RAM AT>ROM
  .pad 0x1030 : AT(0x1030) { BYTE(0xee) }
  .bss : { *(.bss) } >RAM
  /DISCARD/ : { *(.note.gnu.property) }
}
