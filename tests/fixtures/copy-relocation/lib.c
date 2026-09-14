int lib_counter = 100;
char lib_message[32] = "initialized in the library";

void lib_increment(void) {
    lib_counter++;
}

int lib_read(void) {
    return lib_counter;
}
