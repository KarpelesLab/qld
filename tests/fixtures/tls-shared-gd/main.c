#include <pthread.h>
#include <stdio.h>

extern __thread int shared_counter;
extern __thread int shared_zero;
int bump(void);

static void *worker(void *arg) {
    (void)arg;
    int value = bump();
    printf("thread: %d %d\n", value, shared_zero + 1);
    return 0;
}

int main(void) {
    printf("lib: %d\n", bump());
    pthread_t thread;
    if (pthread_create(&thread, 0, worker, 0) != 0)
        return 1;
    pthread_join(thread, 0);
    int value = bump();
    printf("main: %d %d\n", value, shared_counter);
    return 0;
}
