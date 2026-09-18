// See literals_a.c.
int printf(const char *, ...);
const char *name_a(void);
const char *only_a(void);
double scale_a(double);

const char *name_b(void) { return "shared string"; }
double scale_b(double x) { return x * 3.14159265358979; }

int main(void) {
    printf("%s %s %d %s %g\n", name_a(), name_b(), name_a() == name_b(), only_a(),
           scale_a(2.0) + scale_b(1.0));
    return 0;
}
