// An executable that loads a bundle (bundle_plugin.c, linked with
// `-bundle_loader` against this executable) and calls into it; the bundle
// calls back into host_value.
int printf(const char *, ...);
void *dlopen(const char *, int);
void *dlsym(void *, const char *);
char *dlerror(void);

int host_value(void) { return 40; }

int main(int argc, char **argv) {
    if (argc < 2) {
        return 2;
    }
    void *handle = dlopen(argv[1], 2 /* RTLD_NOW */);
    if (!handle) {
        printf("dlopen: %s\n", dlerror());
        return 1;
    }
    int (*entry)(void) = (int (*)(void))dlsym(handle, "bundle_entry");
    if (!entry) {
        printf("dlsym: %s\n", dlerror());
        return 1;
    }
    printf("bundle %d\n", entry());
    return 0;
}
