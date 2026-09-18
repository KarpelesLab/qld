// With relocatable_b.cpp: two C++ objects merged with `-r`
// (`relocatable_output`). Both instantiate `Box<int>::twice` and
// `shared_counter` (weak definitions, one copy kept, with its unwind
// information), throw through them, and use statics and a thread-local.
extern "C" int printf(const char *, ...);

template <typename T> struct Box {
    T value;
    __attribute__((noinline)) T twice() const {
        if (value < 0) {
            throw value;
        }
        return value * 2;
    }
};

inline int shared_counter() {
    static int count = 0;
    return ++count;
}

static int local_a = 5;
static const char *greeting = "hello from a";

int *local_a_ptr() { return &local_a; }
const char *greeting_a() { return greeting; }

int from_a(int x) {
    Box<int> box{x};
    shared_counter();
    return box.twice();
}
