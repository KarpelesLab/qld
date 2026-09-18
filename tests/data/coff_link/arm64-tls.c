/* Thread-local storage without a C runtime: the TLS directory the loader
   reads, and a variable reached through `_tls_index` and SECREL
   relocations. */
typedef unsigned long long u64;

struct tls_directory {
    u64 start, end, index, callbacks;
    unsigned int zero_fill, characteristics;
};

char tls_start __attribute__((section(".tls"))) = 0;
char tls_end __attribute__((section(".tls$ZZZ"))) = 0;
unsigned int _tls_index = 0;
const struct tls_directory _tls_used = {
    (u64)&tls_start, (u64)&tls_end, (u64)&_tls_index, 0, 0, 0,
};

__thread int tls_value = 30;

int bump_tls(int by) {
    tls_value += by;
    return tls_value;
}
