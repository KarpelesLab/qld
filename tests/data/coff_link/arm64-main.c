/* A freestanding Windows ARM64 program: no C runtime, kernel32 only.
   It prints "hello from qld arm64", then "42 7 5 9 42", and exits 0. */
typedef void *HANDLE;
typedef unsigned long DWORD;
__declspec(dllimport) HANDLE __stdcall GetStdHandle(DWORD);
__declspec(dllimport) int __stdcall WriteFile(HANDLE, const void *, DWORD, DWORD *, void *);
__declspec(dllimport) void __stdcall ExitProcess(unsigned int);

extern int far_function(int);   /* arm64-far.s, past the padding */
extern int near_dispatch(int);  /* arm64-near.s: branches that need thunks */
extern int bump_tls(int);       /* arm64-tls.c */

static const char *const messages[] = {"hello from qld arm64\n", " ", "\n"};
int counter = 40;

static void put(const char *text) {
    DWORD length = 0;
    while (text[length])
        length++;
    DWORD written;
    WriteFile(GetStdHandle((DWORD)-11), text, length, &written, 0);
}

static void put_number(int value) {
    char digits[16];
    int at = 15;
    digits[at] = 0;
    do {
        digits[--at] = (char)('0' + value % 10);
        value /= 10;
    } while (value && at > 0);
    put(digits + at);
}

void mainCRTStartup(void) {
    put(messages[0]);
    counter += far_function(1);
    int results[] = {counter, near_dispatch(3), near_dispatch(0), near_dispatch(2), bump_tls(12)};
    int ok = results[0] == 42 && results[1] == 7 && results[2] == 5 && results[3] == 9
             && results[4] == 42;
    for (int i = 0; i < 5; i++) {
        put_number(results[i]);
        put(messages[i == 4 ? 2 : 1]);
    }
    ExitProcess(ok ? 0 : 1);
}
