int pick(void);

int call_from_other(void) {
    return pick();
}

int (*address_from_other(void))(void) {
    return pick;
}
