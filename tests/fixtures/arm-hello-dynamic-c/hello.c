#include <stdio.h>
#include <string.h>

int main(void) {
    const char *text = "hello, dynamic world";
    printf("%s (%zu)\n", text, strlen(text));
    return 0;
}
