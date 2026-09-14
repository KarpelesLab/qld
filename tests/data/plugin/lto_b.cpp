// IR input for the plugin tests: a COMDAT inline function, also in lto_c.cpp.
__attribute__((noinline)) inline int comdat_inline(int x) { return x + 1; }

extern "C" int from_b(int x) { return comdat_inline(x) * 2; }
