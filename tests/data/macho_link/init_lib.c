// A dylib with an `-init` routine (LC_ROUTINES_64), used by init_client.c.
int printf(const char *, ...);

static int ready;

void lib_init(void) {
    ready = 42;
    printf("init ran\n");
}

int lib_ready(void) { return ready; }

// `-alias _lib_ready _lib_ready_alias` gives this function a second name.
