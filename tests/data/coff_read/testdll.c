/* COFF reader fixture: a minimal DLL without the C runtime. */
int dll_data = 42;
const int dll_rodata = 7;

int dll_function(int x) { return x + dll_data; }
int dll_hidden_by_ordinal(void) { return dll_rodata; }

int DllMain(void *instance, unsigned long reason, void *reserved)
{
    (void)instance;
    (void)reason;
    (void)reserved;
    return 1;
}
