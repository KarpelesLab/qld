int square_lto_qld(int x) { return x * x; }

/* Referenced by nothing: LTO removes it. */
int dead_function_lto_qld(int x) { return x - 1; }
