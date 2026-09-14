#pragma once

inline int &counter_qld() {
  static int count = 0;
  return ++count;
}

struct Shape {
  virtual ~Shape() {}
  virtual int sides() const { return 0; }
};

struct Square : Shape {
  int sides() const override { return 4; }
};

template <typename T> T twice_qld(T value) { return value + value; }

extern thread_local int per_thread_qld;

int part1_value();
int part2_value();
const Shape *part1_shape();
const Shape *part2_shape();
