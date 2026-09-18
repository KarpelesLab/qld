#include <stdio.h>

extern int part_b_value_qld(void);

int part_a_table_qld[4] = {1, 2, 3, 4};

int part_a_sum_qld(void) {
    int sum = part_b_value_qld();
    for (int i = 0; i < 4; i++) {
        sum += part_a_table_qld[i];
    }
    return sum;
}

int main(void) {
    printf("sum %d\n", part_a_sum_qld());
    return 0;
}
