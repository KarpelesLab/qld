// A bundle whose reference to host_value is resolved against the loading
// executable (bundle_host.c) with `-bundle_loader`.
int host_value(void);

int bundle_entry(void) { return host_value() + 2; }
