/* Native: calls into the bitcode. */
int bc_called_from_native(int);
int from_native_object(void) { return bc_called_from_native(1); }

__attribute__((weak)) int shared_weak(void) { return 1; }
int native_calls_weak(void) { return shared_weak(); }
