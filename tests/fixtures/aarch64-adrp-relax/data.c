// Data for main.c: one variable next to the code, and one 4 MiB away,
// past what ADR reaches.
int near_qld = 1;
int table_qld[4] = {2, 3, 4, 5};
char padding_qld[4 << 20];
char far_qld[16];

int twice_qld(int n) { return 2 * n; }
int (*callback_qld)(int) = twice_qld;
