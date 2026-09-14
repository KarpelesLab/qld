/* Mach-O reader fixture: 32-bit x86, whose PIC references use scattered
 * GENERIC_RELOC_SECTDIFF/LOCAL_SECTDIFF + PAIR relocations.
 * Build: clang --target=i386-apple-macos10.14 -O1 -c i386.c
 */
static int counter = 5;
int global_data = 9;
extern int external_data;

const char *message(void) { return "i386"; }

int bump(int x) {
    counter += x;
    return counter + global_data + external_data;
}
