/* IR input for the plugin tests: data, a common symbol, a weak definition,
   a weak reference, and references to the other IR files and to main.c. */
int global_data = 42;
int internal_data = 7;
int common_var;
extern int native_value;
extern int from_b(int);
extern int from_c(int);
extern int maybe_missing(void) __attribute__((weak));

__attribute__((weak)) int weak_fn(void) { return 5; }

__attribute__((noinline)) int ir_helper(int x) { return x * internal_data; }

int api(int x) {
  int extra = maybe_missing ? maybe_missing() : 0;
  return ir_helper(x) + from_b(x) + from_c(x) + native_value + common_var +
         extra;
}
