// An executable linked against the greet dylib. Its own weak definition of
// greet_weak does not replace the dylib's (two-level namespace).
int greet(const char *);
extern int greet_count;
extern _Thread_local int greet_tls;
int missing_weak(void) __attribute__((weak_import));

int main(void) {
    int result = greet("dylib");
    result += greet("again");
    if (missing_weak)
        return 10;
    return (greet_count == 2 && result == 2 && greet_tls == 9) ? 0 : 1;
}
