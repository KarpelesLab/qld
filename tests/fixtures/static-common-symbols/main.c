#include <stdio.h>

int shared_counter;
extern char big_common[];
int *counter_from_a(void);
void set_from_b(void);

int main(void) {
    set_from_b();
    printf("shared_counter=%d\n", shared_counter);
    printf("big_common[63]=%c\n", big_common[63]);
    printf("same object: %s\n", counter_from_a() == &shared_counter ? "yes" : "no");
    return 0;
}
