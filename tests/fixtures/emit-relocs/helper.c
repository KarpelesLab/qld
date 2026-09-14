__thread int per_thread_qld = 4;
extern int shared_counter_qld;

__attribute__((noinline)) static int local_add_qld(int x) { return x + shared_counter_qld; }

int helper_qld(int x) { return local_add_qld(x) + 1; }

__attribute__((noinline)) int unused_qld(void) { return 42; }
