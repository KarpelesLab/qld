/* A pointer table in .data: absolute relocations in a non-PIC executable. */

static const char first[] = "hello from _start\n";
static const char second[] = "second message\n";

const char *const messages[] = {first, second, 0};

const char *message(int index) {
    return messages[index];
}
