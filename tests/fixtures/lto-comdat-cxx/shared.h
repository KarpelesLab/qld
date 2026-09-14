#pragma once

inline int &counter() {
    static int value = 0;
    return value;
}

template <typename T> T twice(T v) {
    return v * 2;
}

struct Widget {
    virtual ~Widget() = default;
    virtual int id() const { return 7; }
};

int from_a();
int from_b();
int *counter_address_a();
int *counter_address_b();
const Widget *widget_a();
const Widget *widget_b();
int (*twice_int_a())(int);
int (*twice_int_b())(int);
