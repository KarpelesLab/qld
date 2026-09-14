#include <pthread.h>
#include <stdio.h>
#include <string.h>

__thread int tdata = 42;
__thread long tbss;
static __thread char tarr[32] = "tls";

static void *worker(void *arg) {
    tdata += (int)(long)arg;
    tbss = (long)arg * 2;
    strcat(tarr, "-thread");
    printf("thread: tdata=%d tbss=%ld tarr=%s\n", tdata, tbss, tarr);
    return 0;
}

int main(void) {
    pthread_t thread;
    if (pthread_create(&thread, 0, worker, (void *)1L) != 0)
        return 1;
    pthread_join(thread, 0);
    printf("main: tdata=%d tbss=%ld tarr=%s\n", tdata, tbss, tarr);
    return 0;
}
