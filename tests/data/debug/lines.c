
#include "lines.h"
struct point { int x, y; };

static int twice(int v) { return v * 2; }

int compute(struct point *p, int n) {
  int total = 0;
  for (int i = 0; i < n; i++) {
    total += p[i].x * twice(p[i].y);
    if (total > 1000)
      total = header_helper(total);
  }
  return total;
}

#line 500 "generated.y"
int generated(int a) {
  return a + 42;
}
#line 24 "lines.c"

int caller(void) {
  struct point pts[4] = { {1, 2}, {3, 4}, {5, 6}, {7, 8} };
  int r = compute(pts, 4);
  r += generated(r);
  return r + header_helper(r);
}
