// The C++ part of the `-fuse-ld` suite (tests/macho_link/suite.rs): a
// program built with Apple clang, libc++ and qld against the real SDK.
// Exceptions and RTTI across a dylib boundary, iostreams, the standard
// containers and algorithms, threads, thread-local variables with
// destructors, static initialization, regular expressions and the file
// system library.
#include "cxx_lib.h"

#include <algorithm>
#include <any>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <filesystem>
#include <fstream>
#include <functional>
#include <iostream>
#include <map>
#include <mutex>
#include <numeric>
#include <optional>
#include <regex>
#include <set>
#include <sstream>
#include <thread>
#include <typeinfo>
#include <unistd.h>
#include <unordered_map>
#include <variant>

static int checks;
static int failures;

#define CHECK(cond)                                                                \
    do {                                                                           \
        checks++;                                                                  \
        if (!(cond)) {                                                             \
            failures++;                                                            \
            std::printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);            \
        }                                                                          \
    } while (0)

// Static initialization with dynamic initializers, in order.
static std::vector<int> init_log;
struct Registrar {
    explicit Registrar(int id) { init_log.push_back(id); }
};
static Registrar first(1);
static Registrar second(2);
static const std::map<std::string, int> lookup = {{"one", 1}, {"two", 2}, {"three", 3}};

// A class defined here, derived from the dylib's.
struct Circle : suite::Shape {
    double radius;
    explicit Circle(double radius) : radius(radius) {}
    double area() const override { return 3.0 * radius * radius; }
};

struct LocalError : std::logic_error {
    LocalError() : std::logic_error("local") {}
};

static int destroyed;
struct Tracked {
    int id;
    explicit Tracked(int id) : id(id) {}
    ~Tracked() { destroyed++; }
};

__attribute__((noinline)) static void throw_through(int depth) {
    Tracked tracked(depth);
    if (depth == 0)
        suite::throw_library_error(7);
    throw_through(depth - 1);
}

static thread_local std::string thread_name = "main";
static std::atomic<int> thread_destructors{0};
struct ThreadGuard {
    ~ThreadGuard() { thread_destructors++; }
};
static thread_local ThreadGuard thread_guard;

static void check_exceptions() {
    // Thrown in the dylib through frames of the executable.
    try {
        throw_through(5);
        CHECK(false);
    } catch (const suite::LibraryError &error) {
        CHECK(error.code == 7);
        CHECK(std::string(error.what()) == "from the library");
    }
    CHECK(destroyed == 6);

    // Thrown here, caught in the dylib by base class.
    CHECK(suite::call_and_catch([] { throw LocalError(); }) == 1);
    CHECK(suite::call_and_catch([] { throw std::runtime_error("x"); }) == 2);
    CHECK(suite::call_and_catch([] { throw 42; }) == 42);
    CHECK(suite::call_and_catch([] {}) == 0);

    // Standard library exceptions.
    try {
        (void)std::vector<int>().at(3);
        CHECK(false);
    } catch (const std::out_of_range &) {
        CHECK(true);
    }
    try {
        std::variant<int, std::string> v = 3;
        (void)std::get<std::string>(v);
        CHECK(false);
    } catch (const std::bad_variant_access &) {
        CHECK(true);
    }
    try {
        std::any a = 1;
        (void)std::any_cast<double>(a);
        CHECK(false);
    } catch (const std::bad_any_cast &) {
        CHECK(true);
    }

    // Rethrow and nested exceptions.
    try {
        try {
            throw std::invalid_argument("inner");
        } catch (...) {
            std::throw_with_nested(std::runtime_error("outer"));
        }
    } catch (const std::runtime_error &outer) {
        CHECK(std::string(outer.what()) == "outer");
        try {
            std::rethrow_if_nested(outer);
            CHECK(false);
        } catch (const std::invalid_argument &inner) {
            CHECK(std::string(inner.what()) == "inner");
        }
    }
    std::exception_ptr saved;
    try {
        throw suite::LibraryError("saved", 3);
    } catch (...) {
        saved = std::current_exception();
    }
    try {
        std::rethrow_exception(saved);
    } catch (const suite::LibraryError &error) {
        CHECK(error.code == 3);
    }
}

static void check_rtti() {
    auto square = suite::make_square(3.0);
    CHECK(square->area() == 9.0);
    CHECK(square->name() == "square");
    CHECK(suite::is_square(*square));
    suite::Shape &shape = *square;
    CHECK(typeid(shape) == suite::square_type());
    CHECK(typeid(shape) == typeid(suite::Square));
    CHECK(dynamic_cast<suite::Square *>(square.get()) != nullptr);

    Circle circle(2.0);
    CHECK(circle.area() == 12.0);
    CHECK(circle.name() == "shape");
    CHECK(!suite::is_square(circle));
    CHECK(dynamic_cast<Circle *>(static_cast<suite::Shape *>(&circle)) == &circle);

    suite::Diamond diamond;
    suite::Base &base = diamond;
    CHECK(base.value() == 111);
    CHECK(dynamic_cast<suite::Right *>(&base) != nullptr);
    CHECK(dynamic_cast<suite::Diamond *>(&base) == &diamond);

    // One copy of an inline function's static local across images.
    suite::shared_counter() = 10;
    CHECK(suite::bump_shared_counter() == 11);
    CHECK(suite::shared_counter() == 11);
    CHECK(suite::twice(21) == 42);
    CHECK(suite::library_twice("ab") == "abab");
}

static void check_library() {
    std::vector<int> numbers(100);
    std::iota(numbers.begin(), numbers.end(), 1);
    CHECK(std::accumulate(numbers.begin(), numbers.end(), 0) == 5050);
    std::sort(numbers.begin(), numbers.end(), std::greater<>());
    CHECK(numbers.front() == 100);

    std::unordered_map<std::string, int> counts;
    for (const auto &word : suite::split("a,b,a,c,a,b", ','))
        counts[word]++;
    CHECK(counts["a"] == 3 && counts["b"] == 2 && counts["c"] == 1);
    CHECK(lookup.at("two") == 2);
    std::set<std::string> ordered{"z", "a", "m"};
    CHECK(*ordered.begin() == "a");

    std::ostringstream out;
    out << "value=" << 42 << ' ' << 1.5 << ' ' << std::hex << 255;
    CHECK(out.str() == "value=42 1.5 ff");
    std::istringstream in("7 8.5 word");
    int i = 0;
    double d = 0;
    std::string s;
    in >> i >> d >> s;
    CHECK(i == 7 && d == 8.5 && s == "word");

    std::function<int(int)> square = [](int x) { return x * x; };
    CHECK(square(12) == 144);
    std::optional<int> maybe;
    CHECK(!maybe.has_value() && maybe.value_or(5) == 5);
    auto shared = std::make_shared<std::string>("shared");
    std::weak_ptr<std::string> weak = shared;
    CHECK(weak.lock() && *weak.lock() == "shared");
    shared.reset();
    CHECK(weak.expired());

    std::regex pattern(R"((\w+)@(\w+)\.com)");
    std::smatch match;
    std::string address = "user@example.com";
    CHECK(std::regex_match(address, match, pattern));
    CHECK(match.size() == 3 && match[2] == "example");

    namespace fs = std::filesystem;
    std::error_code error;
    fs::path dir = fs::temp_directory_path(error) /
                   ("qld-suite-cxx-" + std::to_string(::getpid()));
    fs::create_directories(dir, error);
    CHECK(!error);
    {
        std::ofstream file(dir / "data.txt");
        file << "line one\nline two\n";
    }
    std::ifstream file(dir / "data.txt");
    std::string line;
    int lines = 0;
    while (std::getline(file, line))
        lines++;
    CHECK(lines == 2);
    CHECK(fs::file_size(dir / "data.txt") == 18);
    fs::remove_all(dir, error);
    CHECK(!fs::exists(dir));

    auto start = std::chrono::steady_clock::now();
    CHECK(std::chrono::steady_clock::now() >= start);
}

static void check_threads() {
    std::mutex mutex;
    std::condition_variable ready;
    int finished = 0;
    std::atomic<long> total{0};
    std::vector<std::thread> threads;
    for (int t = 0; t < 8; t++) {
        threads.emplace_back([&, t] {
            thread_name = "worker" + std::to_string(t);
            (void)&thread_guard;
            long local = 0;
            for (int i = 0; i < 10000; i++)
                local += i % 7;
            total += local;
            int value = suite::library_thread_local();
            std::lock_guard<std::mutex> lock(mutex);
            if (value != 41 || thread_name != "worker" + std::to_string(t))
                failures++;
            finished++;
            ready.notify_one();
        });
    }
    {
        std::unique_lock<std::mutex> lock(mutex);
        ready.wait(lock, [&] { return finished == 8; });
    }
    for (auto &thread : threads)
        thread.join();
    CHECK(total == 8L * 29994);
    CHECK(thread_name == "main");
    CHECK(thread_destructors == 8);
    CHECK(suite::library_thread_local() == 41);
    CHECK(suite::library_thread_local() == 42);
}

int main() {
    CHECK(init_log.size() == 2 && init_log[0] == 1 && init_log[1] == 2);
    check_exceptions();
    check_rtti();
    check_library();
    check_threads();
    std::cout << "c++ suite: " << checks << " checks, " << failures << " failures" << std::endl;
    return failures == 0 ? 0 : 1;
}
