/* Bitcode for a dylib. */
int api_exported(int x) { return x + 1; }
int api_other(int x) { return x + 2; }
__attribute__((visibility("hidden"))) int api_hidden(int x) { return x + 3; }
int api_uses_hidden(int x) { return api_hidden(x) * 2; }
