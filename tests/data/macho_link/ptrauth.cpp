// arm64e pointer authentication without system headers: signed pointers
// to a local function and to an import, a plain data pointer, a vtable
// (address-diversified entries with discriminators, and a type-info vtable
// import with an addend), a thread-local variable and calls through
// __auth_stubs.
extern "C" int printf(const char *, ...);
extern "C" int puts(const char *);

static int local_function() { return 1; }
int (*local_pointer)() = local_function;
int (*import_pointer)(const char *) = puts;
int data = 3;
int *data_pointer = &data;

struct Base {
    virtual ~Base();
    virtual int value();
};
Base::~Base() {}
int Base::value() { return 42; }

thread_local int tls = 5;

int main() {
    Base *base = new Base;
    printf("ptrauth %d %d %d %d\n", local_pointer(), base->value(), *data_pointer, tls);
    import_pointer("ok");
    delete base;
    return 0;
}
