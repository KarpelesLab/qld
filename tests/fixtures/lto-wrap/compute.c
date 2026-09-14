__attribute__((noinline)) int compute(int x) {
    return x * 10;
}

/* A reference from the defining object is not an undefined reference, so
   --wrap does not redirect it. */
int compute_directly(int x) {
    return compute(x);
}
