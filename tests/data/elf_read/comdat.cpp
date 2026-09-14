// Fixture for tests/elf_read.rs: COMDAT groups from inline functions,
// templates and vtables, plus exception handling tables.

inline int inline_fn(int x) { return x + 1; }

template <typename T> struct Box {
    T value;
    T get() const { return value; }
};

struct Base {
    virtual ~Base() {}
    virtual int f() const { return 1; }
};

struct Derived : Base {
    int f() const override { return 2; }
};

static thread_local int counter = 3;

int thrower(int x)
{
    if (x < 0)
        throw x;
    return x;
}

int use(int y)
{
    Box<int> b{y};
    Box<long> c{y};
    Derived d;
    const Base &base = d;
    counter += base.f();
    try {
        return inline_fn(b.get()) + static_cast<int>(c.get()) + thrower(y);
    } catch (int e) {
        return e + counter;
    }
}
