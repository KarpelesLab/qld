// Thread-local variables: initialized, zero-initialized, and one defined in
// another translation unit (tlv_other.c).
int printf(const char *, ...);

_Thread_local int tlv_initialized = 40;
_Thread_local int tlv_zero;
_Thread_local char tlv_buffer[64] = "tls";
extern _Thread_local long tlv_other;

int main(void) {
    tlv_initialized += 2;
    tlv_zero = 3;
    tlv_other += 1;
    printf("%d %d %s %ld\n", tlv_initialized, tlv_zero, tlv_buffer, tlv_other);
    return tlv_initialized == 42 && tlv_zero == 3 && tlv_other == 6 ? 0 : 1;
}
