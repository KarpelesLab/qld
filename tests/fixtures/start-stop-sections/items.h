#pragma once

struct item {
    const char *name;
    int value;
};

/* Deliberately not `used`: with GCC 11+ that sets SHF_GNU_RETAIN, which would
   keep the section alive by itself. Here only __start_/__stop_ references do. */
#define ITEM(n, v)                                                             \
    const struct item item_##n __attribute__((section("qld_items"))) = {#n, v}
