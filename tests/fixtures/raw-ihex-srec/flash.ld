/* Code crossing 0x08010000, so Intel HEX needs a new extended linear
   address record mid-section, and .data loaded at a separate address. */
ENTRY(_start)
SECTIONS
{
  .text 0x0800fff4 : { *(.text) }
  .data 0x20000000 : AT(0x08020000) { *(.data) }
  /DISCARD/ : { *(.note.gnu.property) }
}
