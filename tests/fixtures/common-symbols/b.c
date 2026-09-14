int shared_counter;
char big_common[64];

void set_from_b(void) {
    shared_counter = 5;
    big_common[63] = 'x';
}
