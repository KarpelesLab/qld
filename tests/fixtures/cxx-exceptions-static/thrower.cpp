#include <cstdio>
#include <stdexcept>
#include <string>

namespace {
struct Guard {
    int depth;
    ~Guard() { std::printf("unwind depth %d\n", depth); }
};
} // namespace

__attribute__((noinline)) void thrower(int depth) {
    Guard guard{depth};
    if (depth == 0)
        throw std::runtime_error("boom at depth " + std::to_string(depth));
    thrower(depth - 1);
}
