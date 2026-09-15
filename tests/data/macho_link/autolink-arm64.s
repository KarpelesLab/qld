// Asks for libc++ through LC_LINKER_OPTION. Put in an archive, it is only
// extracted during resolution, after the command line was searched.
    .linker_option "-lc++"
    .text
    .globl _release
    .p2align 2
_release:
    b __ZdlPv

.subsections_via_symbols
