#include <pthread.h>
#include <stdio.h>

/* Thread-local variables defined in IR; tls_counter_lto_qld is also used
   by native.o. */
_Thread_local int tls_counter_lto_qld = 10;
_Thread_local char tls_name_lto_qld[16] = "main";

int native_bump_lto_qld(void);

static void *worker(void *arg)
{
    (void)arg;
    tls_counter_lto_qld += 5;
    native_bump_lto_qld();
    printf("thread: counter=%d name=%s\n", tls_counter_lto_qld, tls_name_lto_qld);
    return 0;
}

int main(void)
{
    pthread_t thread;
    if (pthread_create(&thread, 0, worker, 0) != 0)
        return 1;
    pthread_join(thread, 0);
    native_bump_lto_qld();
    printf("main: counter=%d name=%s\n", tls_counter_lto_qld, tls_name_lto_qld);
    return 0;
}
