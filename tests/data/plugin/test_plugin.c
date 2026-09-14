/* A linker plugin for testing qld's plugin host (tests/plugin.rs).

   It declares the plugin interface itself, from the ABI, and behaves
   according to its -plugin-opt values:

     fatal-onload    report a fatal message from onload
     onload-error    return an error status from onload
     negotiate       negotiate the API level and report the result
     claim-v2        register the second-version claim handler
     symbols-v2      add symbols with add_symbols_v2
     bad-calls       make invalid calls while claiming and report statuses
     sections        query ELF sections of every offered file
     get-symbols=N   use get_symbols version N (default 3)
     add-file=PATH   add PATH as an input file after all symbols are read
     error-message   report a non-fatal error after all symbols are read
     asr-error       fail the all-symbols-read handler
     threads         report messages from another thread

   A file is claimed if its contents start with "QLDTEST\n". Each further
   line is "<kind> <visibility> <name> [<comdat key>]", with kind and
   visibility as the interface's numbers. Everything the plugin observes is
   reported through info messages, which the tests read back. */

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>

enum { OK = 0, NO_SYMS = 1, BAD_HANDLE = 2, ERR = 3 };
enum { INFO = 0, WARNING = 1, ERROR = 2, FATAL = 3 };

struct input_file {
  const char *name;
  int fd;
  off_t offset;
  off_t filesize;
  void *handle;
};

struct symbol {
  char *name;
  char *version;
#if __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
  char unused, section_kind, symbol_type, def;
#else
  char def, symbol_type, section_kind, unused;
#endif
  int visibility;
  uint64_t size;
  char *comdat_key;
  int resolution;
};

struct section {
  const void *handle;
  unsigned int shndx;
};

typedef int (*claim_fn)(const struct input_file *, int *);
typedef int (*claim_v2_fn)(const struct input_file *, int *, int);
typedef int (*void_fn)(void);
typedef int (*new_input_fn)(const struct input_file *);

struct tv {
  int tag;
  union {
    int val;
    const char *string;
    void *pointer;
  } u;
};

static int (*message)(int, const char *, ...);
static int (*register_claim)(claim_fn);
static int (*register_claim_v2)(claim_v2_fn);
static int (*register_asr)(void_fn);
static int (*register_cleanup)(void_fn);
static int (*register_new_input)(new_input_fn);
static int (*add_symbols)(void *, int, const struct symbol *);
static int (*add_symbols_v2)(void *, int, const struct symbol *);
static int (*get_symbols[4])(const void *, int, struct symbol *);
static int (*get_input_file)(const void *, struct input_file *);
static int (*get_view)(const void *, const void **);
static int (*release_input_file)(const void *);
static int (*add_input_file)(const char *);
static int (*add_input_library)(const char *);
static int (*set_extra_library_path)(const char *);
static int (*section_count)(const void *, unsigned int *);
static int (*section_type)(struct section, unsigned int *);
static int (*section_name)(struct section, char **);
static int (*section_contents)(struct section, const unsigned char **, size_t *);
static int (*section_size)(struct section, uint64_t *);
static int (*get_wrap_symbols)(uint64_t *, const char ***);
static int (*get_api_version)(const char *, const char *, int, int,
                              const char **, const char **);

static int output_kind = -1;
static int gnu_ld_version = -1;
static const char *output_name;
static int opt_claim_v2, opt_symbols_v2, opt_bad_calls, opt_sections;
static int opt_get_symbols = 3, opt_error_message, opt_asr_error, opt_threads;
static const char *opt_add_file;

struct claimed {
  void *handle;
  char *name;
  int count;
  struct symbol *symbols;
};
static struct claimed claimed[64];
static int claimed_count;

static void check_bad_calls(const struct input_file *file) {
  struct symbol sym;
  memset(&sym, 0, sizeof sym);
  message(INFO, "bad add_symbols(-1) = %d", add_symbols(file->handle, -1, &sym));
  message(INFO, "bad add_symbols(null) = %d", add_symbols(file->handle, 1, NULL));
  message(INFO, "bad add_symbols(null name) = %d",
          add_symbols(file->handle, 1, &sym));
  sym.name = "x";
  sym.def = 9;
  message(INFO, "bad add_symbols(kind 9) = %d", add_symbols(file->handle, 1, &sym));
  sym.def = 0;
  message(INFO, "bad add_symbols(handle) = %d",
          add_symbols((void *)(intptr_t)123456, 1, &sym));
  message(INFO, "bad get_symbols(pending) = %d",
          get_symbols[3](file->handle, 0, NULL));
  const void *view;
  message(INFO, "bad get_view(handle) = %d", get_view(NULL, &view));
  message(INFO, "bad get_input_file(null) = %d",
          get_input_file(file->handle, NULL));
  message(INFO, "bad release_input_file(handle) = %d",
          release_input_file((void *)(intptr_t)99999));
  message(INFO, "bad add_input_file(null) = %d", add_input_file(NULL));
  message(INFO, "bad message(null) = %d", message(INFO, NULL));
  message(INFO, "format %5d|%-3s|%x|%c|%%", 42, "ab", 255, 'z');
}

static void check_sections(const struct input_file *file) {
  unsigned int count = 0, type = 0;
  int status = section_count(file->handle, &count);
  if (status != OK) {
    message(INFO, "sections %s: status %d", file->name, status);
    return;
  }
  for (unsigned int i = 1; i < count; i++) {
    struct section s = {file->handle, i};
    char *name = NULL;
    uint64_t size = 0;
    const unsigned char *contents = NULL;
    size_t len = 0;
    if (section_name(s, &name) != OK || section_type(s, &type) != OK ||
        section_size(s, &size) != OK ||
        section_contents(s, &contents, &len) != OK)
      continue;
    if (strcmp(name, ".text") == 0)
      message(INFO, "section %u %s type %u size %u len %u", i, name, type,
              (unsigned)size, (unsigned)len);
    free(name);
  }
  struct section bad = {file->handle, 100000};
  message(INFO, "bad section_type(index) = %d", section_type(bad, &type));
}

static int claim_common(const struct input_file *file, int *claimed_out) {
  *claimed_out = 0;
  if (opt_sections)
    check_sections(file);

  const void *view;
  if (get_view(file->handle, &view) != OK)
    return ERR;
  const char *text = view;
  size_t size = (size_t)file->filesize;
  if (size < 8 || memcmp(text, "QLDTEST\n", 8) != 0)
    return OK;

  if (opt_bad_calls)
    check_bad_calls(file);

  char *copy = malloc(size - 8 + 1);
  memcpy(copy, text + 8, size - 8);
  copy[size - 8] = 0;

  struct claimed *c = &claimed[claimed_count++];
  c->handle = file->handle;
  c->name = strdup(file->name);
  c->symbols = calloc(64, sizeof *c->symbols);
  c->count = 0;
  char *save = NULL;
  for (char *line = strtok_r(copy, "\n", &save); line && c->count < 64;
       line = strtok_r(NULL, "\n", &save)) {
    int def, vis;
    char name[256], key[256];
    key[0] = 0;
    if (sscanf(line, "%d %d %255s %255s", &def, &vis, name, key) < 3)
      continue;
    struct symbol *s = &c->symbols[c->count++];
    s->name = strdup(name);
    s->version = NULL;
    s->def = (char)def;
    s->visibility = vis;
    s->size = def == 4 ? 8 : 0;
    s->comdat_key = key[0] ? strdup(key) : NULL;
    s->symbol_type = opt_symbols_v2 ? (def == 4 ? 2 : 1) : 0;
    s->section_kind = opt_symbols_v2 && def == 4 ? 1 : 0;
  }
  free(copy);
  int status = opt_symbols_v2 && add_symbols_v2
                   ? add_symbols_v2(c->handle, c->count, c->symbols)
                   : add_symbols(c->handle, c->count, c->symbols);
  if (status != OK)
    return ERR;
  *claimed_out = 1;
  return OK;
}

static int claim_v1(const struct input_file *file, int *claimed_out) {
  return claim_common(file, claimed_out);
}

static int claim_v2(const struct input_file *file, int *claimed_out,
                    int known_used) {
  message(INFO, "claim_v2 %s known_used=%d", file->name, known_used);
  return claim_common(file, claimed_out);
}

static void *thread_message(void *arg) {
  (void)arg;
  message(WARNING, "warning from a plugin thread");
  return NULL;
}

static int all_symbols_read(void) {
  for (int i = 0; i < claimed_count; i++) {
    struct claimed *c = &claimed[i];
    int status = get_symbols[opt_get_symbols](c->handle, c->count, c->symbols);
    char line[4096];
    int len = snprintf(line, sizeof line, "resolve %s status=%d", c->name, status);
    for (int j = 0; j < c->count && len < 4000; j++)
      len += snprintf(line + len, sizeof line - len, " %s=%d", c->symbols[j].name,
                      c->symbols[j].resolution);
    message(INFO, "%s", line);
    message(INFO, "too many get_symbols = %d",
            get_symbols[opt_get_symbols](c->handle, c->count + 1, c->symbols));
  }
  if (get_wrap_symbols) {
    uint64_t count = 0;
    const char **names = NULL;
    get_wrap_symbols(&count, &names);
    for (uint64_t i = 0; i < count; i++)
      message(INFO, "wrap %s", names[i]);
  }
  if (opt_threads) {
    pthread_t thread;
    pthread_create(&thread, NULL, thread_message, NULL);
    pthread_join(thread, NULL);
  }
  if (opt_add_file)
    add_input_file(opt_add_file);
  add_input_library("m");
  set_extra_library_path("/qld/test/lib");
  if (opt_error_message)
    message(ERROR, "an error from %s", "the plugin");
  return opt_asr_error ? ERR : OK;
}

static int cleanup(void) {
  message(INFO, "cleanup ran");
  return OK;
}

static int new_input(const struct input_file *file) {
  message(INFO, "new input %s", file->name);
  return OK;
}

int onload(struct tv *tv) {
  int fatal = 0, error = 0, negotiate = 0;
  for (; tv->tag != 0; tv++) {
    switch (tv->tag) {
    case 3: output_kind = tv->u.val; break;
    case 4: {
      const char *o = tv->u.string;
      if (strcmp(o, "fatal-onload") == 0) fatal = 1;
      else if (strcmp(o, "onload-error") == 0) error = 1;
      else if (strcmp(o, "negotiate") == 0) negotiate = 1;
      else if (strcmp(o, "claim-v2") == 0) opt_claim_v2 = 1;
      else if (strcmp(o, "symbols-v2") == 0) opt_symbols_v2 = 1;
      else if (strcmp(o, "bad-calls") == 0) opt_bad_calls = 1;
      else if (strcmp(o, "sections") == 0) opt_sections = 1;
      else if (strncmp(o, "get-symbols=", 12) == 0) opt_get_symbols = atoi(o + 12);
      else if (strncmp(o, "add-file=", 9) == 0) opt_add_file = o + 9;
      else if (strcmp(o, "error-message") == 0) opt_error_message = 1;
      else if (strcmp(o, "asr-error") == 0) opt_asr_error = 1;
      else if (strcmp(o, "threads") == 0) opt_threads = 1;
      break;
    }
    case 5: register_claim = tv->u.pointer; break;
    case 6: register_asr = tv->u.pointer; break;
    case 7: register_cleanup = tv->u.pointer; break;
    case 8: add_symbols = tv->u.pointer; break;
    case 9: get_symbols[1] = tv->u.pointer; break;
    case 10: add_input_file = tv->u.pointer; break;
    case 11: message = tv->u.pointer; break;
    case 12: get_input_file = tv->u.pointer; break;
    case 13: release_input_file = tv->u.pointer; break;
    case 14: add_input_library = tv->u.pointer; break;
    case 15: output_name = tv->u.string; break;
    case 16: set_extra_library_path = tv->u.pointer; break;
    case 17: gnu_ld_version = tv->u.val; break;
    case 18: get_view = tv->u.pointer; break;
    case 19: section_count = tv->u.pointer; break;
    case 20: section_type = tv->u.pointer; break;
    case 21: section_name = tv->u.pointer; break;
    case 22: section_contents = tv->u.pointer; break;
    case 25: get_symbols[2] = tv->u.pointer; break;
    case 28: get_symbols[3] = tv->u.pointer; break;
    case 30: section_size = tv->u.pointer; break;
    case 31: register_new_input = tv->u.pointer; break;
    case 32: get_wrap_symbols = tv->u.pointer; break;
    case 33: add_symbols_v2 = tv->u.pointer; break;
    case 34: get_api_version = tv->u.pointer; break;
    case 35: register_claim_v2 = tv->u.pointer; break;
    }
  }
  if (!message || !register_claim || !get_symbols[1] || !get_symbols[2] ||
      !get_symbols[3])
    return ERR;
  message(INFO, "onload output=%d ld=%d name=%s", output_kind, gnu_ld_version,
          output_name ? output_name : "(none)");
  if (fatal) {
    message(FATAL, "fatal from %s (code %d)", "qld-test", 42);
    return OK;
  }
  if (error)
    return ERR;
  if (negotiate) {
    const char *id = NULL, *version = NULL;
    int level = get_api_version("qld-test", "1.0", 0, 1, &id, &version);
    message(INFO, "api level %d linker %s", level, id ? id : "(null)");
  }
  if (opt_claim_v2)
    register_claim_v2(claim_v2);
  else
    register_claim(claim_v1);
  register_asr(all_symbols_read);
  register_cleanup(cleanup);
  register_new_input(new_input);
  message(INFO, "bad register(null) = %d", register_cleanup(NULL));
  return OK;
}
