// The C++ suite's dylib (libsuite_cxx.dylib): key functions of a class
// hierarchy, exceptions thrown to and caught from the executable, an
// inline function's static local, thread-local variables.
#include "cxx_lib.h"

#include <sstream>
#include <typeinfo>

namespace suite {

Shape::~Shape() = default;
std::string Shape::name() const { return "shape"; }

double Square::area() const { return side * side; }
std::string Square::name() const { return "square"; }

std::unique_ptr<Shape> make_square(double side) { return std::make_unique<Square>(side); }

void throw_library_error(int code) { throw LibraryError("from the library", code); }

int call_and_catch(void (*callback)()) {
    try {
        callback();
    } catch (const std::logic_error &error) {
        return 1;
    } catch (const std::exception &error) {
        return 2;
    } catch (int value) {
        return value;
    }
    return 0;
}

bool is_square(const Shape &shape) { return dynamic_cast<const Square *>(&shape) != nullptr; }

const std::type_info &square_type() { return typeid(Square); }

int bump_shared_counter() { return ++shared_counter(); }

std::vector<std::string> split(const std::string &text, char separator) {
    std::vector<std::string> out;
    std::istringstream stream(text);
    std::string item;
    while (std::getline(stream, item, separator))
        out.push_back(item);
    return out;
}

namespace {
struct Counted {
    int value = 40;
    ~Counted() { value = -1; }
};
thread_local Counted library_local;
} // namespace

int library_thread_local() { return ++library_local.value; }

std::string library_twice(const std::string &text) { return twice(text); }

} // namespace suite
