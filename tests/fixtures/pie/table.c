struct entry {
    const char *name;
    int (*fn)(int);
    int arg;
};

static int square(int x) {
    return x * x;
}

const struct entry table[] = {
    {"one", square, 1},
    {"two", square, 2},
    {"three", square, 3},
    {0, 0, 0},
};
