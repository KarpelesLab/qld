/* A minimal x32 runtime for the freestanding test programs: the entry
 * point, output through system calls, the TLS block of a static
 * executable, and the relocations a static PIE applies to itself (relative
 * ones, then IFUNCs; a static executable has only its IFUNCs, between
 * `__rela_iplt_start` and `__rela_iplt_end`). With -DDYNAMIC the dynamic
 * linker has done all of that. */

typedef unsigned int u32;
typedef unsigned long long u64;

typedef struct {
    unsigned char e_ident[16];
    unsigned short e_type, e_machine;
    u32 e_version, e_entry, e_phoff, e_shoff, e_flags;
    unsigned short e_ehsize, e_phentsize, e_phnum, e_shentsize, e_shnum, e_shstrndx;
} Ehdr;

typedef struct {
    u32 p_type, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz, p_flags, p_align;
} Phdr;

typedef struct {
    int d_tag;
    u32 d_val;
} Dyn;

typedef struct {
    u32 r_offset, r_info;
    int r_addend;
} Rela;

#define HIDDEN __attribute__((visibility("hidden")))
#define WEAK __attribute__((weak))

extern const Ehdr __ehdr_start HIDDEN;
#ifdef SELF_RELOC
extern Dyn _DYNAMIC[] HIDDEN;
#else
extern const Rela __rela_iplt_start[] WEAK HIDDEN;
extern const Rela __rela_iplt_end[] WEAK HIDDEN;
#endif

/* x32 system calls are the x86-64 ones with bit 30 set. */
#define SYS(n) (0x40000000 | (n))

static long syscall2(long n, long a, long b) {
    long ret;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"(n), "D"(a), "S"(b)
                     : "rcx", "r11", "memory");
    return ret;
}

static long syscall3(long n, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"(n), "D"(a), "S"(b), "d"(c)
                     : "rcx", "r11", "memory");
    return ret;
}

void put_str(const char *s) {
    u32 n = 0;
    while (s[n])
        n++;
    syscall3(SYS(1), 1, (long)s, n);
}

void put_num(long long value) {
    char buf[24];
    int at = sizeof buf;
    int negative = value < 0;
    u64 v = negative ? -(u64)value : (u64)value;
    buf[--at] = 0;
    do {
        buf[--at] = '0' + (char)(v % 10);
        v /= 10;
    } while (v);
    if (negative)
        buf[--at] = '-';
    put_str(buf + at);
}

int main(void);

#ifndef DYNAMIC
#define R_X86_64_RELATIVE 8
#define R_X86_64_IRELATIVE 37
#define DT_NULL 0
#define DT_PLTRELSZ 2
#define DT_RELA 7
#define DT_RELASZ 8
#define DT_JMPREL 23
#define PT_LOAD 1
#define PT_TLS 7

/* The load address: where the first PT_LOAD, which maps the ELF header,
 * was linked to be. Everything here is `%rip`-relative, so it works before
 * relocation. */
static u32 load_base(void) {
    const Ehdr *ehdr = &__ehdr_start;
    const Phdr *phdr = (const Phdr *)((const char *)ehdr + ehdr->e_phoff);
    for (int i = 0; i < ehdr->e_phnum; i++)
        if (phdr[i].p_type == PT_LOAD && phdr[i].p_offset == 0)
            return (u32)ehdr - phdr[i].p_vaddr;
    return 0;
}

static void apply(const Rela *rela, u32 size, u32 base, int type) {
    for (u32 i = 0; i < size / sizeof(Rela); i++) {
        u32 *place = (u32 *)(base + rela[i].r_offset);
        if ((rela[i].r_info & 0xff) != (u32)type)
            continue;
        if (type == R_X86_64_RELATIVE)
            *place = base + rela[i].r_addend;
        else
            *place = ((u32(*)(void))(base + rela[i].r_addend))();
    }
}

__attribute__((noinline, used)) void relocate(void) {
#ifdef SELF_RELOC
    /* A static PIE: its own relative relocations, then its IFUNCs. */
    u32 base = load_base();
    u32 rela = 0, relasz = 0, jmprel = 0, pltrelsz = 0;
    for (const Dyn *d = _DYNAMIC; d->d_tag != DT_NULL; d++) {
        if (d->d_tag == DT_RELA)
            rela = d->d_val;
        else if (d->d_tag == DT_RELASZ)
            relasz = d->d_val;
        else if (d->d_tag == DT_JMPREL)
            jmprel = d->d_val;
        else if (d->d_tag == DT_PLTRELSZ)
            pltrelsz = d->d_val;
    }
    apply((const Rela *)(base + rela), relasz, base, R_X86_64_RELATIVE);
    apply((const Rela *)(base + rela), relasz, base, R_X86_64_IRELATIVE);
    apply((const Rela *)(base + jmprel), pltrelsz, base, R_X86_64_IRELATIVE);
#else
    /* A static executable: only its IFUNCs, at their link-time places. */
    u32 size = (u32)__rela_iplt_end - (u32)__rela_iplt_start;
    apply(__rela_iplt_start, size, 0, R_X86_64_IRELATIVE);
#endif
}

/* The static TLS block, variant II: the block ends at the thread pointer,
 * which points at the thread control block, whose first word is the
 * thread pointer itself. */
static char tls_area[4096] __attribute__((aligned(64)));

__attribute__((noinline, used)) void tls_setup(void) {
    const Ehdr *ehdr = &__ehdr_start;
    const Phdr *phdr = (const Phdr *)((const char *)ehdr + ehdr->e_phoff);
    u32 base = load_base();
    for (int i = 0; i < ehdr->e_phnum; i++) {
        if (phdr[i].p_type != PT_TLS)
            continue;
        u32 align = phdr[i].p_align ? phdr[i].p_align : 1;
        u32 size = (phdr[i].p_memsz + align - 1) & -align;
        u32 tp = ((u32)tls_area + size + align - 1) & -align;
        volatile char *block = (volatile char *)(tp - size);
        const char *image = (const char *)(base + phdr[i].p_vaddr);
        for (u32 j = 0; j < size; j++)
            block[j] = j < phdr[i].p_filesz ? image[j] : 0;
        *(volatile u32 *)tp = tp;
        /* ARCH_SET_FS */
        syscall2(SYS(158), 0x1002, tp);
    }
}
#endif

__attribute__((noinline, used)) void start_c(void) {
#ifndef DYNAMIC
    relocate();
    tls_setup();
#endif
    int status = main();
    /* exit_group */
    syscall2(SYS(231), status, 0);
}

__asm__(".text\n"
        ".globl _start\n"
        ".type _start, @function\n"
        "_start:\n"
        "\txorl %ebp, %ebp\n"
        "\tandq $-16, %rsp\n"
        "\tcall start_c\n"
        "\thlt\n"
        ".size _start, .-_start\n");
