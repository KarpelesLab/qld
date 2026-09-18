// The C suite's bundle, loaded with dlopen: it calls back into the
// executable that loads it (linked with `-bundle_loader`) and into the
// suite's dylib.
#include "c_lib.h"

int suite_plugin_entry(int x) { return suite_c_host_value() + suite_c_triple(x); }
