/* Mach-O reader fixture: atomization inputs (no system headers).
 * Build: clang --target=arm64-apple-macos13 -O1 -fcommon -c atoms.c
 */
#pragma comment(lib, "qldtest")

int common_var;
int common_array[64];
__attribute__((weak)) int weak_var = 7;
_Thread_local int tlv_var = 3;
_Thread_local int tlv_bss;

static const char *strings[] = {"alpha", "beta", "gamma", "alpha"};
static float f4 = 1.5f;
static double d8 = 2.25;

__attribute__((noinline)) static int local_helper(int x) { return x * 3 + tlv_var; }

__attribute__((weak)) int weak_function(int x) { return local_helper(x) + weak_var; }

__attribute__((used)) static void kept(void) {}

__attribute__((cold, noinline)) int cold_function(int x) { return x - 1; }

int string_length(int index) {
    const char *s = strings[index & 3];
    int n = 0;
    while (s[n])
        n++;
    return n + (int)f4 + (int)d8 + common_var + common_array[index & 63] + tlv_bss;
}

double scale(double x) { return x * 3.14159; }
float fscale(float x) { return x * 1.2345f; }

int main(void) { return weak_function(1) + string_length(2) + cold_function(3); }
