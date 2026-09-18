#include <stdio.h>
#include <pthread.h>

/* Local-exec / initial-exec in the executable. */
__thread int local_counter = 5;
static __thread int static_counter = 7;
extern __thread int lib_counter;       /* defined in the shared library */
int lib_get(void);
int lib_bump_ld(void);

static void *worker(void *arg) {
    (void)arg;
    local_counter += 100;
    static_counter += 200;
    lib_counter += 300;
    printf("thread: %d %d %d %d\n", local_counter, static_counter, lib_counter, lib_get());
    return 0;
}

int main(void) {
    pthread_t t;
    pthread_create(&t, 0, worker, 0);
    pthread_join(t, 0);
    local_counter += 1;
    static_counter += 2;
    lib_counter += 3;
    printf("main: %d %d %d %d %d\n", local_counter, static_counter, lib_counter, lib_get(),
           lib_bump_ld());
    return 0;
}
