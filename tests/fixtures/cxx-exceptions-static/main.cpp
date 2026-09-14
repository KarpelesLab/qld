#include <cstdio>
#include <stdexcept>

void thrower(int depth);

int main() {
    try {
        thrower(2);
    } catch (const std::exception &e) {
        std::printf("caught: %s\n", e.what());
    }
    try {
        throw 42;
    } catch (int value) {
        std::printf("caught int %d\n", value);
    }
    std::printf("done\n");
    return 0;
}
