#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum {
  CIELO_RUNTIME_ABI_VERSION_MAJOR = 1,
  CIELO_RUNTIME_ABI_VERSION_MINOR = 0,
  CIELO_RUNTIME_ABI_VERSION_PATCH = 0,
  CIELO_RUNTIME_ABI_VERSION = (CIELO_RUNTIME_ABI_VERSION_MAJOR << 16) |
                              (CIELO_RUNTIME_ABI_VERSION_MINOR << 8) |
                              CIELO_RUNTIME_ABI_VERSION_PATCH
};
/* ABI policy: major=breaking layout/signature changes, minor=additive
 * compatible, patch=behavior-only fixes. */

typedef enum {
  CV_UNIT = 0,
  CV_BOOL = 1,
  CV_INT = 2,
  CV_FLOAT = 3,
  CV_CHAR = 4,
  CV_STRING = 5,
  CV_CTOR = 6
} CieloTag;

typedef struct CieloValue CieloValue;

enum {
  CIELO_ARC_FLAG_IMMORTAL = 1u << 0,
  CIELO_ARC_FLAG_ORC_PINNED = 1u << 1
};

typedef struct {
  uint32_t refcount;
  uint32_t flags;
  uint32_t orc_color;
  uint32_t reserved0;
} CieloArcHeader;

#define CIELO_ARC_HEADER_INIT(REFCOUNT, FLAGS)                                \
  {                                                                            \
    .refcount = (REFCOUNT), .flags = (FLAGS), .orc_color = 0u, .reserved0 = 0u \
  }
#define CIELO_ARC_IMMORTAL_HEADER CIELO_ARC_HEADER_INIT(0u, CIELO_ARC_FLAG_IMMORTAL)
#define CIELO_ARC_OWNED_HEADER CIELO_ARC_HEADER_INIT(1u, 0u)

typedef struct {
  CieloArcHeader arc;
  const char *ty;
  const char *variant;
  size_t argc;
  CieloValue *fields;
} CieloCtor;

struct CieloValue {
  CieloTag tag;
  union {
    bool b;
    int64_t i;
    double f;
    uint32_t c;
    const char *s;
    CieloCtor *ctor;
  } as;
};

typedef struct CieloEvidence CieloEvidence;
typedef struct CieloContinuation CieloContinuation;
typedef struct CieloClauseEntry CieloClauseEntry;

typedef CieloValue (*CieloClauseFn)(CieloEvidence *evidence,
                                    CieloContinuation *continuation,
                                    size_t argc, const CieloValue *args);

struct CieloClauseEntry {
  uint32_t op_symbol;
  CieloClauseFn clause;
};

struct CieloEvidence {
  uint32_t abi_version;
  uint32_t effect;
  uint32_t capability_id;
  uint32_t clause_count;
  const CieloClauseEntry *clauses;
  void *captures;
  void *reserved0;
  void *reserved1;
};

struct CieloContinuation {
  uint32_t abi_version;
  uint32_t state;
  void *payload;
  CieloValue (*resume_once)(void *payload, CieloValue value);
  void *reserved0;
  void *reserved1;
};

typedef struct {
  uint32_t effect;
  uint32_t capability_id;
  CieloEvidence *evidence;
} CieloHandlerFrame;

enum { CIELO_HANDLER_STACK_MAX = 64 };
static CieloHandlerFrame g_cielo_handlers[CIELO_HANDLER_STACK_MAX];
static size_t g_cielo_handler_depth = 0;
static uint32_t g_cielo_next_capability_id = 1;

#define CIELO_CALL_PURE(expr) (expr)
#define CIELO_CALL_DIRECT(expr) (expr)
#define CIELO_CALL_CONTROL(expr) (expr)

typedef struct {
  uint64_t ctor_allocations;
  uint64_t ctor_frees;
  uint64_t retain_calls;
  uint64_t release_calls;
  uint64_t release_last_calls;
} CieloArcStats;

static CieloArcStats g_cielo_arc_stats = {0};

static inline uint32_t cielo_runtime_abi_version(void) {
  return (uint32_t)CIELO_RUNTIME_ABI_VERSION;
}

#define CV_MAKE(TAG, MEMBER, VALUE)                                            \
  ((CieloValue){.tag = (TAG), .as.MEMBER = (VALUE)})

static inline CieloValue cv_unit(void) { return (CieloValue){.tag = CV_UNIT}; }
static inline CieloValue cv_bool(int x) { return CV_MAKE(CV_BOOL, b, x != 0); }
static inline CieloValue cv_int(int64_t x) { return CV_MAKE(CV_INT, i, x); }
static inline CieloValue cv_float(double x) { return CV_MAKE(CV_FLOAT, f, x); }
static inline CieloValue cv_char(uint32_t x) { return CV_MAKE(CV_CHAR, c, x); }
static inline CieloValue cv_string(const char *s) {
  return CV_MAKE(CV_STRING, s, s);
}

static inline void cielo_arc_stats_reset(void) {
  memset(&g_cielo_arc_stats, 0, sizeof(g_cielo_arc_stats));
}

static inline CieloArcStats cielo_arc_stats_snapshot(void) {
  return g_cielo_arc_stats;
}

static inline bool cielo_arc_is_managed(CieloValue value) {
  return value.tag == CV_CTOR && value.as.ctor != NULL;
}

static inline bool cielo_arc_is_immortal_ctor(const CieloCtor *ctor) {
  return ctor != NULL && (ctor->arc.flags & CIELO_ARC_FLAG_IMMORTAL) != 0u;
}

static inline void cielo_arc_retain(CieloValue value) {
  if (!cielo_arc_is_managed(value))
    return;
  CieloCtor *ctor = value.as.ctor;
  if (cielo_arc_is_immortal_ctor(ctor))
    return;
  uint32_t count = ctor->arc.refcount;
  if (count == 0u || count == UINT32_MAX)
    return;
  ctor->arc.refcount = count + 1u;
  g_cielo_arc_stats.retain_calls++;
}

static inline bool cielo_arc_dec_is_last(CieloValue value) {
  if (!cielo_arc_is_managed(value))
    return false;
  CieloCtor *ctor = value.as.ctor;
  if (cielo_arc_is_immortal_ctor(ctor))
    return false;
  if (ctor->arc.refcount == 0u)
    return false;
  ctor->arc.refcount--;
  return ctor->arc.refcount == 0u;
}

static inline void cielo_arc_destroy_and_dispose(CieloValue value);

static inline void cielo_arc_release(CieloValue value) {
  if (!cielo_arc_is_managed(value))
    return;
  g_cielo_arc_stats.release_calls++;
  if (!cielo_arc_dec_is_last(value))
    return;
  g_cielo_arc_stats.release_last_calls++;
  cielo_arc_destroy_and_dispose(value);
}

static inline void cielo_arc_destroy_and_dispose(CieloValue value) {
  if (!cielo_arc_is_managed(value))
    return;
  CieloCtor *ctor = value.as.ctor;
  if (cielo_arc_is_immortal_ctor(ctor))
    return;
  CieloValue *fields = ctor->fields;
  size_t argc = ctor->argc;
  ctor->fields = NULL;
  ctor->argc = 0u;
  for (size_t i = 0; i < argc; i++) {
    cielo_arc_release(fields[i]);
  }
  if (fields != NULL) {
    free(fields);
  }
  g_cielo_arc_stats.ctor_frees++;
  free(ctor);
}

static inline bool cielo_ctor_is_variant(CieloValue value,
                                         const char *variant) {
  if (value.tag != CV_CTOR || value.as.ctor == NULL)
    return false;
  if (value.as.ctor->variant == NULL || variant == NULL)
    return false;
  return strcmp(value.as.ctor->variant, variant) == 0;
}

static inline CieloValue cielo_ctor_field(CieloValue value, size_t index) {
  if (value.tag != CV_CTOR || value.as.ctor == NULL)
    return cv_unit();
  if (index >= value.as.ctor->argc || value.as.ctor->fields == NULL)
    return cv_unit();
  return value.as.ctor->fields[index];
}

static inline uint32_t cielo_handler_push(uint32_t effect) {
  if (g_cielo_handler_depth >= CIELO_HANDLER_STACK_MAX)
    return 0;
  uint32_t capability_id = g_cielo_next_capability_id++;
  if (capability_id == 0) {
    capability_id = g_cielo_next_capability_id++;
  }
  if (g_cielo_next_capability_id == 0) {
    g_cielo_next_capability_id = 1;
  }
  g_cielo_handlers[g_cielo_handler_depth].effect = effect;
  g_cielo_handlers[g_cielo_handler_depth].capability_id = capability_id;
  g_cielo_handlers[g_cielo_handler_depth].evidence = NULL;
  g_cielo_handler_depth++;
  return capability_id;
}

static inline uint32_t
cielo_handler_push_with_evidence(uint32_t effect, CieloEvidence *evidence) {
  uint32_t capability_id = cielo_handler_push(effect);
  if (capability_id == 0)
    return 0;
  if (evidence != NULL) {
    evidence->abi_version = cielo_runtime_abi_version();
    evidence->effect = effect;
    evidence->capability_id = capability_id;
    g_cielo_handlers[g_cielo_handler_depth - 1].evidence = evidence;
  }
  return capability_id;
}

static inline void cielo_handler_pop(uint32_t capability_id) {
  if (capability_id == 0)
    return;
  while (g_cielo_handler_depth > 0) {
    g_cielo_handler_depth--;
    if (g_cielo_handlers[g_cielo_handler_depth].capability_id == capability_id)
      return;
  }
}

static inline uint32_t cielo_handler_find_capability(uint32_t effect) {
  for (size_t i = g_cielo_handler_depth; i > 0; i--) {
    if (g_cielo_handlers[i - 1].effect == effect) {
      return g_cielo_handlers[i - 1].capability_id;
    }
  }
  return 0;
}

static inline bool cielo_handler_active(uint32_t effect) {
  return cielo_handler_find_capability(effect) != 0;
}

static inline bool cv_truthy(CieloValue v) {
  switch (v.tag) {
  case CV_BOOL:
    return v.as.b;
  case CV_INT:
    return v.as.i != 0;
  case CV_FLOAT:
    return v.as.f != 0.0;
  case CV_UNIT:
    return false;
  default:
    return true;
  }
}

static inline CieloValue cv_neg(CieloValue a) { return cv_int(-a.as.i); }
static inline CieloValue cv_not(CieloValue a) { return cv_bool(!cv_truthy(a)); }
static inline CieloValue cv_add(CieloValue a, CieloValue b) {
  return cv_int(a.as.i + b.as.i);
}
static inline CieloValue cv_sub(CieloValue a, CieloValue b) {
  return cv_int(a.as.i - b.as.i);
}
static inline CieloValue cv_mul(CieloValue a, CieloValue b) {
  return cv_int(a.as.i * b.as.i);
}
static inline CieloValue cv_div(CieloValue a, CieloValue b) {
  return cv_int(b.as.i == 0 ? 0 : a.as.i / b.as.i);
}
static inline CieloValue cv_mod(CieloValue a, CieloValue b) {
  return cv_int(b.as.i == 0 ? 0 : a.as.i % b.as.i);
}
static inline CieloValue cv_eq(CieloValue a, CieloValue b) {
  if (a.tag != b.tag)
    return cv_bool(0);
  switch (a.tag) {
  case CV_UNIT:
    return cv_bool(1);
  case CV_BOOL:
    return cv_bool(a.as.b == b.as.b);
  case CV_INT:
    return cv_bool(a.as.i == b.as.i);
  case CV_FLOAT:
    return cv_bool(a.as.f == b.as.f);
  case CV_CHAR:
    return cv_bool(a.as.c == b.as.c);
  case CV_STRING:
    return cv_bool(a.as.s == b.as.s);
  }
  return cv_bool(0);
}
static inline CieloValue cv_ne(CieloValue a, CieloValue b) {
  CieloValue eq = cv_eq(a, b);
  return cv_bool(!eq.as.b);
}
static inline CieloValue cv_lt(CieloValue a, CieloValue b) {
  return cv_bool(a.as.i < b.as.i);
}
static inline CieloValue cv_le(CieloValue a, CieloValue b) {
  return cv_bool(a.as.i <= b.as.i);
}
static inline CieloValue cv_gt(CieloValue a, CieloValue b) {
  return cv_bool(a.as.i > b.as.i);
}
static inline CieloValue cv_ge(CieloValue a, CieloValue b) {
  return cv_bool(a.as.i >= b.as.i);
}
static inline CieloValue cv_and(CieloValue a, CieloValue b) {
  return cv_bool(cv_truthy(a) && cv_truthy(b));
}
static inline CieloValue cv_or(CieloValue a, CieloValue b) {
  return cv_bool(cv_truthy(a) || cv_truthy(b));
}

static inline const char *cielo_cstr0(const char *s) { return s ? s : ""; }

#define CASE_PUTS(TAG, STR_EXPR)                                               \
  case TAG:                                                                    \
    puts((STR_EXPR));                                                          \
    break

#define CASE_PRINTF(TAG, FMT, ...)                                             \
  case TAG:                                                                    \
    printf(FMT "\n", __VA_ARGS__);                                             \
    break

static inline void cv_print(CieloValue v) {
  switch (v.tag) {
    CASE_PUTS(CV_UNIT, "()");
    CASE_PUTS(CV_BOOL, v.as.b ? "true" : "false");
    CASE_PRINTF(CV_INT, "%lld", (long long)v.as.i);
    CASE_PRINTF(CV_FLOAT, "%f", v.as.f);
    CASE_PRINTF(CV_CHAR, "%c", (int)v.as.c);
    CASE_PUTS(CV_STRING, cielo_cstr0(v.as.s));

  case CV_CTOR:
    if (!v.as.ctor) {
      puts("<ctor>");
      break;
    }
    printf("<%s.%s/%zu>\n", cielo_cstr0(v.as.ctor->ty),
           cielo_cstr0(v.as.ctor->variant), v.as.ctor->argc);
    break;

  default:
    puts("<value>");
    break;
  }
}

static CieloValue cielo_perform_scoped(uint32_t effect,
                                       uint32_t expected_capability_id,
                                       uint32_t op_symbol, const char *op,
                                       size_t argc, const CieloValue *args);

static inline bool cielo_dispatch_with_evidence(CieloEvidence *evidence,
                                                uint32_t op_symbol, size_t argc,
                                                const CieloValue *args,
                                                CieloValue *out) {
  if (evidence == NULL || out == NULL) {
    return false;
  }
  if (evidence->abi_version != cielo_runtime_abi_version()) {
    return false;
  }
  if (evidence->clauses == NULL || evidence->clause_count == 0) {
    return false;
  }
  for (uint32_t i = 0; i < evidence->clause_count; i++) {
    const CieloClauseEntry *entry = &evidence->clauses[i];
    if (entry->op_symbol == op_symbol && entry->clause != NULL) {
      *out = entry->clause(evidence, NULL, argc, args);
      return true;
    }
  }
  return false;
}

static CieloValue cielo_perform(uint32_t effect, uint32_t op_symbol,
                                const char *op, size_t argc,
                                const CieloValue *args) {
  return cielo_perform_scoped(effect, 0, op_symbol, op, argc, args);
}

static CieloValue cielo_perform_scoped(uint32_t effect,
                                       uint32_t expected_capability_id,
                                       uint32_t op_symbol, const char *op,
                                       size_t argc, const CieloValue *args) {
  CieloValue dispatched = cv_unit();
  if (expected_capability_id != 0) {
    for (size_t i = g_cielo_handler_depth; i > 0; i--) {
      if (g_cielo_handlers[i - 1].effect == effect &&
          g_cielo_handlers[i - 1].capability_id == expected_capability_id) {
        CieloEvidence *evidence = g_cielo_handlers[i - 1].evidence;
        if (cielo_dispatch_with_evidence(evidence, op_symbol, argc, args,
                                         &dispatched)) {
          return dispatched;
        }
        return cv_unit();
      }
    }
    // Scoped performs must not fall back to another handler instance with the
    // same effect label.
    return cv_unit();
  }
  for (size_t i = g_cielo_handler_depth; i > 0; i--) {
    if (g_cielo_handlers[i - 1].effect == effect) {
      CieloEvidence *evidence = g_cielo_handlers[i - 1].evidence;
      if (cielo_dispatch_with_evidence(evidence, op_symbol, argc, args,
                                       &dispatched)) {
        return dispatched;
      }
      return cv_unit();
    }
  }
#ifdef CIELO_OP_SYMBOL_PRINT
  if (op_symbol == CIELO_OP_SYMBOL_PRINT && argc > 0 && args != NULL) {
    cv_print(args[0]);
    return cv_unit();
  }
#endif
  if (op && strcmp(op, "print") == 0 && argc > 0 && args != NULL) {
    cv_print(args[0]);
    return cv_unit();
  }
  return cv_unit();
}

static CieloValue cielo_make_ctor(const char *ty, const char *variant,
                                  size_t argc, const CieloValue *fields) {
  CieloCtor *ctor = (CieloCtor *)malloc(sizeof(CieloCtor));
  if (ctor == NULL)
    return cv_unit();

  ctor->arc = (CieloArcHeader)CIELO_ARC_OWNED_HEADER;
  ctor->ty = ty;
  ctor->variant = variant;
  ctor->argc = argc;
  ctor->fields = NULL;

  if (argc > 0) {
    ctor->fields = (CieloValue *)malloc(sizeof(CieloValue) * argc);
    if (ctor->fields == NULL) {
      free(ctor);
      return cv_unit();
    }
    if (fields != NULL) {
      memcpy(ctor->fields, fields, sizeof(CieloValue) * argc);
      for (size_t i = 0; i < argc; i++) {
        cielo_arc_retain(ctor->fields[i]);
      }
    } else {
      for (size_t i = 0; i < argc; i++) {
        ctor->fields[i] = cv_unit();
      }
    }
  }

  g_cielo_arc_stats.ctor_allocations++;
  CieloValue out = {.tag = CV_CTOR};
  out.as.ctor = ctor;
  return out;
}
