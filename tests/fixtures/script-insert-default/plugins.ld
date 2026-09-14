/* Adds an output section to the default layout instead of replacing it. */
SECTIONS
{
  .plugins :
  {
    __plugins_start = .;
    KEEP(*(SORT(.plugins.*)))
    __plugins_end = .;
  }
}
INSERT AFTER .data;
