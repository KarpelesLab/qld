// Hello world without system headers, so it compiles for macOS without an
// SDK. Exercises a stub call, a data pointer (a rebase), a string literal and
// a static.
int printf(const char *, ...);

static int counter = 3;
int answer = 42;
int *answer_ptr = &answer;

int main(void) {
    printf("hello from qld %d %d\n", counter, *answer_ptr);
    return *answer_ptr == 42 ? 0 : 1;
}
