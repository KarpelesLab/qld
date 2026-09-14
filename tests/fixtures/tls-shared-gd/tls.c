__thread int shared_counter = 10;
__thread int shared_zero;

int bump(void) {
    shared_zero += 100;
    return ++shared_counter;
}
