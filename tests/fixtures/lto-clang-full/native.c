int ir_callee_lto_qld(int x);

int native_add_lto_qld(int a, int b) { return a + b; }

int native_calls_ir_lto_qld(int x) { return ir_callee_lto_qld(x) * 2; }
