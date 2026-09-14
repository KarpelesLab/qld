// COFF reader fixture: clang in MSVC mode, C++ features.
#pragma comment(lib, "fixturelib")
#pragma comment(linker, "/alternatename:alias_symbol=real_symbol")
#pragma comment(linker, "/include:forced_symbol")

template <typename T> struct Box {
    T value;
    T get() const { return value; }
};

inline int inline_function(int x) { return x * 2; }

static int counter;
int real_symbol;

__attribute__((weak)) int weak_function() { return 1; }
extern int weak_import() __attribute__((weak));

thread_local int thread_value = 7;

__declspec(dllexport) int exported_function(int x)
{
    Box<int> box{x};
    Box<long long> wide{x};
    return box.get() + (int)wide.get() + inline_function(x) + counter + thread_value
         + (weak_import ? weak_import() : 0);
}

__declspec(dllexport) int exported_data = 3;

int (*address_taken)() = weak_function;
