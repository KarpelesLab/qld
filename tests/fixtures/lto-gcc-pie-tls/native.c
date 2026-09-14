extern _Thread_local int tls_counter_lto_qld;

int native_bump_lto_qld(void) { return ++tls_counter_lto_qld; }
