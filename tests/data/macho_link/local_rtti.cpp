// An exception of a class in an anonymous namespace. On arm64 its type
// info points to its (local) name with the top bit set, as the C++ ABI for
// non-unique type names asks: `__ZTS… + 0x8000000000000000`
// (`local_type_info`).
extern "C" int printf(const char *, ...);

namespace {
struct Local {
    int value;
};

__attribute__((noinline)) void raise(int value) { throw Local{value}; }
} // namespace

int main() {
    try {
        raise(7);
    } catch (const Local &local) {
        printf("local %d\n", local.value);
        return 0;
    }
    return 1;
}
