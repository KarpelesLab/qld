__attribute__((noinline)) int native_function_lto_qld(int x) { return x + 99; }

/* Unreferenced: --gc-sections removes it from the native object. */
__attribute__((noinline)) int native_dead_function_lto_qld(int x) { return x - 1; }

int native_dead_data_lto_qld[100] = {1};
