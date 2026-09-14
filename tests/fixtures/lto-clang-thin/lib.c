#include <stdio.h>

extern void exe_callback_lto_qld(int);

static int calls;

int lib_internal_helper_lto_qld(int x) { return x * 3; }

int lib_api_lto_qld(int x)
{
    calls++;
    exe_callback_lto_qld(calls);
    return lib_internal_helper_lto_qld(x) + 1;
}

int lib_calls_lto_qld(void) { return calls; }
