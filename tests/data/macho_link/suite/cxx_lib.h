// Interface of the C++ suite's dylib (cxx_lib.cpp).
#pragma once

#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

namespace suite {

// Thrown by the dylib, caught by the executable (and the reverse).
struct LibraryError : std::runtime_error {
    int code;
    LibraryError(const std::string &what, int code) : std::runtime_error(what), code(code) {}
};

// A polymorphic hierarchy split between the dylib and the executable:
// dynamic_cast and typeid must agree across images.
struct Shape {
    virtual ~Shape();
    virtual double area() const = 0;
    virtual std::string name() const;
};

struct Square : Shape {
    double side;
    explicit Square(double side) : side(side) {}
    double area() const override;
    std::string name() const override;
};

// Virtual inheritance.
struct Base {
    int base_value = 1;
    virtual ~Base() = default;
    virtual int value() const { return base_value; }
};
struct Left : virtual Base {
    int value() const override { return base_value + 10; }
};
struct Right : virtual Base {};
struct Diamond : Left, Right {
    int value() const override { return Left::value() + 100; }
};

// An inline function with a static local: one copy (a weak definition
// coalesced across images at load time).
inline int &shared_counter() {
    static int counter = 0;
    return counter;
}

// A template instantiated in both images.
template <typename T> T twice(T value) { return value + value; }

std::unique_ptr<Shape> make_square(double side);
void throw_library_error(int code);
int call_and_catch(void (*callback)());
bool is_square(const Shape &shape);
const std::type_info &square_type();
int bump_shared_counter();
std::vector<std::string> split(const std::string &text, char separator);
int library_thread_local();
std::string library_twice(const std::string &text);

} // namespace suite
