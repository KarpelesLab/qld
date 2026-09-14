/* COFF reader fixture: MinGW GCC C features. */
static int local_helper(int x) { return x + 1; }

__attribute__((weak)) int weak_definition(void) { return 2; }
extern int weak_reference(void) __attribute__((weak));

int common_symbol;
__attribute__((aligned(32))) int aligned_common;

__thread int thread_local_value = 3;

__attribute__((section(".custom_long_section_name"))) int in_long_section = 4;

__declspec(dllexport) int exported_function(int a)
{
    return local_helper(a) + (weak_reference ? weak_reference() : 0) + thread_local_value;
}

__declspec(dllexport) int exported_data = 5;
