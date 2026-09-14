#include <cstdio>
#include <stdexcept>
#include <string>

struct BoomQld {
    int code;
};

__attribute__((noinline)) int thrower_qld(int n) {
    if (n > 2) {
        throw BoomQld{n};
    }
    return n;
}

__attribute__((noinline)) int rethrow_qld(int n) {
    try {
        return thrower_qld(n);
    } catch (const BoomQld &b) {
        throw std::runtime_error("boom " + std::to_string(b.code));
    }
}

int main() {
    try {
        rethrow_qld(5);
    } catch (const std::runtime_error &e) {
        std::printf("caught %s\n", e.what());
        return 0;
    }
    std::printf("no throw\n");
    return 1;
}
