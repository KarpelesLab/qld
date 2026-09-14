#include <stdio.h>

int strip_marker_function_qld(void) {
    return 7;
}

int main(void) {
    printf("stripped: %d\n", strip_marker_function_qld());
    return 0;
}
