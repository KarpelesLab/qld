// IR input for the plugin tests: the second copy of the COMDAT function.
__attribute__((noinline)) inline int comdat_inline(int x) { return x + 1; }

extern "C" int from_c(int x) { return comdat_inline(x) + 3; }
