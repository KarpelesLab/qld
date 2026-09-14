#include <cstdio>
#include <typeinfo>

#include "shared.h"

int main() {
  int a = part1_value();
  int b = part2_value();
  std::printf("part1=%d part2=%d\n", a, b);
  std::printf("counter=%d\n", counter_qld());
  std::printf("sides=%d %d\n", part1_shape()->sides(), part2_shape()->sides());
  std::printf("same vtable type: %s\n",
              typeid(*part1_shape()) == typeid(*part2_shape()) ? "yes" : "no");
  return 0;
}
