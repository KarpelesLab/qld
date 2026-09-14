/* Explicit program headers: a read-only headers+rodata segment, a code
   segment and a data segment, each with its own flags. */
ENTRY(_start)
PHDRS
{
  headers PT_PHDR PHDRS;
  rodata PT_LOAD FILEHDR PHDRS FLAGS(4);
  text PT_LOAD FLAGS(5);
  data PT_LOAD FLAGS(6);
  stack PT_GNU_STACK FLAGS(6);
}
SECTIONS
{
  . = 0x10000000 + SIZEOF_HEADERS;
  .rodata : { *(.rodata*) } :headers :rodata
  msg_size = msg_end - msg_start;

  . = ALIGN(0x1000);
  .text : SUBALIGN(4)
  {
    *(.text)
    *(SORT(.text.*))
  } :text =0xcccccccc

  . = ALIGN(0x1000) + (. & (0x1000 - 1));
  .data :
  {
    __table_start = .;
    KEEP(*(SORT_BY_NAME(.table.*)))
    __table_end = .;
    counter = .;
    QUAD(DEFINED(missing_symbol) ? 100 : (__table_end - __table_start) / 8 * 10)
    PROVIDE(unused_provide = .);
  } :data
  .bss : { *(.bss) *(COMMON) } :data
  ASSERT(SIZEOF(.data) == 32, "unexpected .data size")
  /DISCARD/ : { *(.unused) *(.note.gnu.property) *(.comment) }
}
