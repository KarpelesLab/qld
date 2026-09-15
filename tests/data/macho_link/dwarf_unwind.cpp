// An exception thrown through a frame that only DWARF call frame
// information describes (call_through, in dwarf-<arch>.s).
extern "C" int printf(const char *, ...);
extern "C" void call_through(void (*)());

static void thrower() { throw 7; }

int main() {
    try {
        call_through(thrower);
    } catch (int value) {
        printf("dwarf %d\n", value);
        return value == 7 ? 0 : 1;
    }
    return 2;
}
