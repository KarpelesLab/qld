// Mach-O reader fixture: C++ exceptions (compact unwind, __eh_frame and
// __gcc_except_tab). No system headers.
// Build: clang++ --target=arm64-apple-macos13 -O1 -c eh.cpp
//        clang++ --target=arm64-apple-macos13 -O1 -femit-dwarf-unwind=always -c eh.cpp
struct Error {
    int code;
};

__attribute__((noinline)) int may_throw(int x) {
    if (x < 0)
        throw Error{x};
    return x * 2;
}

int catches(int x) {
    try {
        return may_throw(x);
    } catch (const Error &e) {
        return e.code;
    }
}

struct Guard {
    int *p;
    ~Guard() { *p += 1; }
};

int cleans_up(int x) {
    int counter = 0;
    {
        Guard g{&counter};
        may_throw(x);
    }
    return counter;
}

inline int inline_weak(int x) { return x + 1; }

int uses_inline(int x) { return inline_weak(x) + catches(x) + cleans_up(x); }
