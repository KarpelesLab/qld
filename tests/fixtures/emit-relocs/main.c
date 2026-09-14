#include <stdio.h>

extern __thread int per_thread_qld;
int shared_counter_qld = 3;
static const char *const names_qld[] = {"alpha", "beta", "gamma"};

int helper_qld(int x);
int (*helper_pointer_qld)(int) = helper_qld;

__attribute__((noinline)) static int local_twice_qld(int x) { return x * 2; }

int main(void) {
  int total = helper_pointer_qld(shared_counter_qld) + local_twice_qld(per_thread_qld);
  printf("%s %d\n", names_qld[total % 3], total);
  return 0;
}
