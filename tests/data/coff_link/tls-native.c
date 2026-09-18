/* Native TLS, as Clang compiles `__thread` for MinGW (GCC uses emulated
   TLS): the variable is reached through `_tls_index` and a SECREL
   relocation into `.tls`. No headers, so any Clang can build it. */
__thread int native_tls = 7;

int bump_native_tls(void) {
    return ++native_tls;
}
