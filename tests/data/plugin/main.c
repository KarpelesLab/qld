/* Native object for the plugin tests. Prints 174. */
#include <stdio.h>

extern int global_data;
extern int common_var;
int api(int);
int weak_fn(void);

int native_value = 100;

int main(void) {
  common_var = 1;
  printf("%d\n", api(2) + global_data + weak_fn());
  return 0;
}
