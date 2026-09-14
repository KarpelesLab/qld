/* Fixture for tests/elf_read.rs: a shared object with symbol versions. */

int puts(const char *);

int v1_fn(void) { return 1; }
int v2_fn(void) { return puts("v2") + 2; }

int old_impl(void) { return 10; }
int new_impl(void) { return 20; }
__asm__(".symver old_impl,versioned@VERS_1");
__asm__(".symver new_impl,versioned@@VERS_2");

int shared_data = 7;
__thread int shared_tls = 1;

int get_tls(void) { return shared_tls; }

void *ptrs[] = { &shared_data, v1_fn, v2_fn, &ptrs };
