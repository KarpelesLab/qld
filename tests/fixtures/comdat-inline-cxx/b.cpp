#include "shared.h"

static Widget widget;

int from_b() {
    ++counter();
    return twice(twice(widget.id())) + 3;
}

int *counter_address_b() {
    return &counter();
}

const Widget *widget_b() {
    return &widget;
}

int (*twice_int_b())(int) {
    return &twice<int>;
}
