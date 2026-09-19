#include <iostream>
#include <map>
#include <memory>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

struct Base {
    virtual ~Base() = default;
    virtual std::string name() const = 0;
};

struct Derived : Base {
    std::string n;
    explicit Derived(std::string s) : n(std::move(s)) {}
    std::string name() const override { return "derived:" + n; }
};

thread_local int tls_calls = 0;

static int thrower(int depth) {
    ++tls_calls;
    if (depth == 0) throw std::runtime_error("bottom reached");
    return thrower(depth - 1) + 1;
}

int main() {
    std::vector<std::unique_ptr<Base>> items;
    for (int i = 0; i < 3; ++i) items.push_back(std::make_unique<Derived>(std::to_string(i)));
    std::map<std::string, int> counts;
    for (auto &item : items) counts[item->name()]++;
    for (auto &[k, v] : counts) std::cout << k << "=" << v << "\n";
    try {
        thrower(5);
    } catch (const std::exception &e) {
        std::cout << "caught: " << e.what() << " after " << tls_calls << " calls\n";
    }
    int other = 0;
    std::thread t([&] {
        try {
            thrower(2);
        } catch (const std::runtime_error &) {
            other = tls_calls;
        }
    });
    t.join();
    std::cout << "thread calls: " << other << "\n";
    return 0;
}
