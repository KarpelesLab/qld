#include "shared.h"

static Widget widget;

int from_a() {
    ++counter();
    return twice(widget.id()) + 1;
}

int *counter_address_a() {
    return &counter();
}

const Widget *widget_a() {
    return &widget;
}

int (*twice_int_a())(int) {
    return &twice<int>;
}
