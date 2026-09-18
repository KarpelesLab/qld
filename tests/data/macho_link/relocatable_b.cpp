// See relocatable_a.cpp.
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

int from_a(int x);
int *local_a_ptr();
const char *greeting_a();

static thread_local int tls_b = 3;
static char flag_b;

__attribute__((noinline)) void set_flag() { flag_b = 1; }

int main() {
    int total = 0;
    try {
        total += from_a(-1);
    } catch (int value) {
        total += value * 100;
    }
    Box<int> box{21};
    total += box.twice();
    total += from_a(4);
    tls_b += 4;
    set_flag();
    printf("%s total %d counter %d local %d tls %d flag %d\n", greeting_a(), total,
           shared_counter(), *local_a_ptr(), tls_b, flag_b);
    return 0;
}
