// A dylib with an `-init` routine (LC_ROUTINES_64), used by init_client.c.
// The test links it with `-alias _lib_ready _lib_ready_alias`, which gives
// lib_ready a second name.
int printf(const char *, ...);

static int ready;

void lib_init(void) {
    ready = 42;
    printf("init ran\n");
}

int lib_ready(void) { return ready; }
