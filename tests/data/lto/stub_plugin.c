/* A stand-in LTO plugin for the ELF driver's LTO tests (tests/lto.rs).

   It claims fake IR files: LLVM bitcode magic ("BC\xC0\xDE"), then
   "QLDLTO\n", then one symbol per line, "<kind> <visibility> <name>
   [<comdat key>]", with kind and visibility as the plugin interface's
   numbers. qld recognizes the magic as bitcode and offers the file; the
   plugin reports what it sees through info messages, which qld prints as
   "qld: note: ..." lines for the tests to read:

     claim <name>@<offset> known_used=<0|1> claimed=<0|1>
     resolve <name>@<offset> status=<status> <symbol>=<resolution> ...
     wrap <symbol>
     new-input <name>
     cleanup

   Options (-plugin-opt):

     add-file=PATH       add PATH after all symbols are read (repeatable)
     add-library=NAME    add -lNAME after all symbols are read (repeatable)
     library-path=DIR    set_extra_library_path(DIR)
     error               report an error after all symbols are read
     output-kind         report the output kind in onload */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>

enum { OK = 0, NO_SYMS = 1, ERR = 3 };
enum { INFO = 0, ERROR = 2 };

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

struct tv {
  int tag;
  union {
    int val;
    const char *string;
    void *pointer;
  } u;
};

typedef int (*claim_v2_fn)(const struct input_file *, int *, int);
typedef int (*void_fn)(void);
typedef int (*new_input_fn)(const struct input_file *);

static int (*message)(int, const char *, ...);
static int (*register_claim_v2)(claim_v2_fn);
static int (*register_asr)(void_fn);
static int (*register_cleanup)(void_fn);
static int (*register_new_input)(new_input_fn);
static int (*add_symbols)(void *, int, const struct symbol *);
static int (*get_symbols_v3)(const void *, int, struct symbol *);
static int (*get_view)(const void *, const void **);
static int (*add_input_file)(const char *);
static int (*add_input_library)(const char *);
static int (*set_extra_library_path)(const char *);
static int (*get_wrap_symbols)(uint64_t *, const char ***);

#define MAX_OPTIONS 16
static const char *add_files[MAX_OPTIONS];
static int add_file_count;
static const char *add_libraries[MAX_OPTIONS];
static int add_library_count;
static const char *library_path;
static int report_error;
static int output_kind = -1;
static int report_output_kind;

#define MAX_CLAIMS 64
#define MAX_SYMBOLS 64
struct claimed {
  void *handle;
  char name[512];
  int count;
  struct symbol symbols[MAX_SYMBOLS];
};
static struct claimed claims[MAX_CLAIMS];
static int claim_count;

static const char magic[] = "BC\xc0\xde" "QLDLTO\n";

static int claim(const struct input_file *file, int *claimed_out, int known_used) {
  *claimed_out = 0;
  const void *view;
  if (get_view(file->handle, &view) != OK)
    return ERR;
  size_t size = (size_t)file->filesize;
  size_t header = sizeof magic - 1;
  if (size < header || memcmp(view, magic, header) != 0 ||
      claim_count == MAX_CLAIMS) {
    message(INFO, "claim %s@%d known_used=%d claimed=0", file->name,
            (int)file->offset, known_used);
    return OK;
  }
  struct claimed *c = &claims[claim_count++];
  c->handle = file->handle;
  snprintf(c->name, sizeof c->name, "%s@%d", file->name, (int)file->offset);
  char *text = malloc(size - header + 1);
  memcpy(text, (const char *)view + header, size - header);
  text[size - header] = 0;
  char *save = NULL;
  for (char *line = strtok_r(text, "\n", &save); line && c->count < MAX_SYMBOLS;
       line = strtok_r(NULL, "\n", &save)) {
    int def, vis;
    char name[256], key[256];
    key[0] = 0;
    if (sscanf(line, "%d %d %255s %255s", &def, &vis, name, key) < 3)
      continue;
    struct symbol *s = &c->symbols[c->count++];
    memset(s, 0, sizeof *s);
    s->name = strdup(name);
    s->def = (char)def;
    s->visibility = vis;
    s->size = def == 4 ? 8 : 0;
    s->comdat_key = key[0] ? strdup(key) : NULL;
  }
  free(text);
  if (add_symbols(c->handle, c->count, c->symbols) != OK)
    return ERR;
  *claimed_out = 1;
  message(INFO, "claim %s known_used=%d claimed=1", c->name, known_used);
  return OK;
}

static int all_symbols_read(void) {
  for (int i = 0; i < claim_count; i++) {
    struct claimed *c = &claims[i];
    int status = get_symbols_v3(c->handle, c->count, c->symbols);
    char line[8192];
    int len = snprintf(line, sizeof line, "resolve %s status=%d", c->name, status);
    for (int j = 0; j < c->count && len < 8000; j++)
      len += snprintf(line + len, sizeof line - len, " %s=%d", c->symbols[j].name,
                      status == OK ? c->symbols[j].resolution : -1);
    message(INFO, "%s", line);
  }
  if (get_wrap_symbols) {
    uint64_t count = 0;
    const char **names = NULL;
    if (get_wrap_symbols(&count, &names) == OK)
      for (uint64_t i = 0; i < count; i++)
        message(INFO, "wrap %s", names[i]);
  }
  if (library_path)
    set_extra_library_path(library_path);
  for (int i = 0; i < add_file_count; i++)
    add_input_file(add_files[i]);
  for (int i = 0; i < add_library_count; i++)
    add_input_library(add_libraries[i]);
  if (report_error)
    message(ERROR, "code generation failed in the stub plugin");
  return OK;
}

static int cleanup(void) {
  message(INFO, "cleanup");
  return OK;
}

static int new_input(const struct input_file *file) {
  message(INFO, "new-input %s", file->name);
  return OK;
}

int onload(struct tv *tv) {
  for (; tv->tag != 0; tv++) {
    switch (tv->tag) {
    case 3: output_kind = tv->u.val; break;
    case 4: {
      const char *o = tv->u.string;
      if (strncmp(o, "add-file=", 9) == 0 && add_file_count < MAX_OPTIONS)
        add_files[add_file_count++] = o + 9;
      else if (strncmp(o, "add-library=", 12) == 0 && add_library_count < MAX_OPTIONS)
        add_libraries[add_library_count++] = o + 12;
      else if (strncmp(o, "library-path=", 13) == 0)
        library_path = o + 13;
      else if (strcmp(o, "error") == 0)
        report_error = 1;
      else if (strcmp(o, "output-kind") == 0)
        report_output_kind = 1;
      break;
    }
    case 6: register_asr = tv->u.pointer; break;
    case 7: register_cleanup = tv->u.pointer; break;
    case 8: add_symbols = tv->u.pointer; break;
    case 10: add_input_file = tv->u.pointer; break;
    case 11: message = tv->u.pointer; break;
    case 14: add_input_library = tv->u.pointer; break;
    case 16: set_extra_library_path = tv->u.pointer; break;
    case 18: get_view = tv->u.pointer; break;
    case 28: get_symbols_v3 = tv->u.pointer; break;
    case 31: register_new_input = tv->u.pointer; break;
    case 32: get_wrap_symbols = tv->u.pointer; break;
    case 35: register_claim_v2 = tv->u.pointer; break;
    }
  }
  if (!message || !register_claim_v2 || !register_asr || !add_symbols ||
      !get_symbols_v3 || !get_view || !add_input_file || !add_input_library)
    return ERR;
  if (report_output_kind)
    message(INFO, "onload output-kind=%d", output_kind);
  register_claim_v2(claim);
  register_asr(all_symbols_read);
  if (register_cleanup)
    register_cleanup(cleanup);
  if (register_new_input)
    register_new_input(new_input);
  return OK;
}
