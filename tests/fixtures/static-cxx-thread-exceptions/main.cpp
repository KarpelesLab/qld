#include <iostream>
#include <map>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

thread_local int counter = 5;
static std::map<std::string, int> table = {{"a", 1}, {"b", 2}};

int work(int i) {
    if (i == 3)
        throw std::runtime_error("three");
    counter += i;
    return counter;
}

int main() {
    std::vector<std::thread> threads;
    for (int t = 0; t < 4; t++)
        threads.emplace_back([t] {
            try {
                work(t);
            } catch (const std::exception &e) {
                std::cout << "caught " << e.what() << "\n";
            }
        });
    for (auto &t : threads)
        t.join();
    std::cout << "table " << table["a"] + table["b"] << " counter " << counter << "\n";
    return 0;
}
