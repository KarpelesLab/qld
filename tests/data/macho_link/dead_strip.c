// Dead stripping: unused functions, data and strings disappear; used ones,
// `used` ones and what they reference stay.
int printf(const char *, ...);

int unused_data = 12345;
int used_data = 7;

__attribute__((noinline)) int unused_function(void) { return unused_data; }
__attribute__((noinline)) static int used_helper(int x) { return x + used_data; }
__attribute__((used)) static int kept_by_attribute(void) { return 3; }
__attribute__((noinline)) int unused_caller(void) { return unused_function() + 1; }

int main(void) {
    printf("%s %d\n", "stripped", used_helper(35));
    return 0;
}
