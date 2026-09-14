#include "shared.h"

thread_local int per_thread_qld = 5;
int common_counter_qld;

__attribute__((noinline)) static int private_helper(int x) { return x * 3; }

int part1_value() {
  counter_qld();
  return private_helper(twice_qld(2)) + per_thread_qld;
}

const Shape *part1_shape() {
  static Square square;
  return &square;
}
