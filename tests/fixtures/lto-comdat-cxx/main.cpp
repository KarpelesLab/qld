#include <cstdio>

#include "shared.h"

// The vtable pointer is the first word of a polymorphic object.
static void *vptr(const Widget *w) {
    return *static_cast<void **>(static_cast<void *>(const_cast<Widget *>(w)));
}

int main() {
    int a = from_a();
    int b = from_b();
    std::printf("from_a=%d from_b=%d\n", a, b);
    std::printf("counter=%d\n", counter());
    std::printf("same counter: %s\n", counter_address_a() == counter_address_b() ? "yes" : "no");
    std::printf("same vtable: %s\n", vptr(widget_a()) == vptr(widget_b()) ? "yes" : "no");
    std::printf("same template: %s\n", twice_int_a() == twice_int_b() ? "yes" : "no");
    return 0;
}
