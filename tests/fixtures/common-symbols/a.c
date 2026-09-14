int shared_counter;
char big_common[16];

int *counter_from_a(void) {
    return &shared_counter;
}
