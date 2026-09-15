// C++ exceptions without system headers: throwing through several frames,
// a catch by type, a cleanup that runs during unwinding, and a rethrow.
extern "C" int printf(const char *, ...);

struct Error {
    int code;
};

static int cleanups = 0;

struct Guard {
    ~Guard() { cleanups++; }
};

__attribute__((noinline)) static int depth(int n) {
    Guard guard;
    if (n == 0)
        throw Error{42};
    return depth(n - 1) + 1;
}

__attribute__((noinline)) static int rethrow() {
    try {
        depth(3);
    } catch (Error &) {
        throw;
    }
    return 0;
}

int main() {
    int code = 0;
    try {
        depth(5);
    } catch (const Error &e) {
        code = e.code;
    }
    try {
        rethrow();
    } catch (const Error &e) {
        code += e.code;
    } catch (...) {
        code = -1;
    }
    printf("caught %d after %d cleanups\n", code, cleanups);
    return code == 84 && cleanups == 10 ? 0 : 1;
}
