// With literals_b.c: the same C string and floating-point constant in two
// objects, which the linker merges (`literal_deduplication`).
const char *name_a(void) { return "shared string"; }
const char *only_a(void) { return "only in a"; }
double scale_a(double x) { return x * 3.14159265358979; }
