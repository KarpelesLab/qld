#include <stdexcept>

#include "shared.h"

__attribute__((noinline)) static int private_helper(int x) { return x + 100; }

int part2_value() {
  counter_qld();
  try {
    if (twice_qld(3) == 6)
      throw std::runtime_error("thrown in part2");
  } catch (const std::exception &) {
    return private_helper(twice_qld(1)) + per_thread_qld;
  }
  return 0;
}

const Shape *part2_shape() {
  static Square square;
  return &square;
}
