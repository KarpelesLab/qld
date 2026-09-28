int table_qld[4] = {1, 2, 3, 4};

int add_qld(int a, int b) {
    return a + b;
}

int pick_qld(int index) {
    return table_qld[index & 3];
}
