// Calls far_function (far.c) and puts across a 130 MiB filler function.
int puts(const char *);
int far_function(int);

int main(void) {
    puts("near");
    return far_function(41) == 42 ? 0 : 1;
}
