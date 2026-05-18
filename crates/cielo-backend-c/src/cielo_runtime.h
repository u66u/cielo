/* Codegen pastes this file into the generated translation unit and defines
 * CIELO_RUNTIME_IMPL ahead of the paste, so that unit owns the one definition
 * of the runtime's mutable state and out-of-line functions. Every other unit --
 * the copy the driver writes to disk, which tests and FFI consumers include by
 * name -- sees declarations only and links against it. Two units that both
 * define CIELO_RUNTIME_IMPL collide at link time; that is deliberate, because
 * the alternative is each of them silently getting a private handler stack. */
#ifndef CIELO_RUNTIME_H
#define CIELO_RUNTIME_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum {
  CIELO_RUNTIME_ABI_VERSION_MAJOR = 2,
  CIELO_RUNTIME_ABI_VERSION_MINOR = 1,
  CIELO_RUNTIME_ABI_VERSION_PATCH = 0,
  CIELO_RUNTIME_ABI_VERSION = (CIELO_RUNTIME_ABI_VERSION_MAJOR << 16) |
                              (CIELO_RUNTIME_ABI_VERSION_MINOR << 8) |
                              CIELO_RUNTIME_ABI_VERSION_PATCH
};
/* ABI policy: major=breaking layout/signature changes, minor=additive
 * compatible, patch=behavior-only fixes. */

/* Unrecoverable faults abort here. A sentinel return would be
 * indistinguishable from a real result. */
_Noreturn void cielo_trap(const char *what);

/* Same, but names the operation that faulted. */
_Noreturn void cielo_trap_op(const char *what, const char *op);

typedef enum {
  CV_UNIT = 0,
  CV_BOOL = 1,
  CV_INT = 2,
  CV_FLOAT = 3,
  CV_CHAR = 4,
  CV_STRING = 5,
  CV_CTOR = 6,
  CV_CLOSURE = 7
} CieloTag;

typedef struct CieloValue CieloValue;
typedef struct CieloClosure CieloClosure;

enum {
  CIELO_ARC_FLAG_IMMORTAL = 1u << 0
};

typedef struct {
  uint32_t refcount;
  uint32_t flags;
} CieloArcHeader;

#define CIELO_ARC_HEADER_INIT(REFCOUNT, FLAGS) \
  { .refcount = (REFCOUNT), .flags = (FLAGS) }
#define CIELO_ARC_IMMORTAL_HEADER CIELO_ARC_HEADER_INIT(0u, CIELO_ARC_FLAG_IMMORTAL)
#define CIELO_ARC_OWNED_HEADER CIELO_ARC_HEADER_INIT(1u, 0u)

typedef struct {
  CieloArcHeader arc;
  /* `ty` and `variant` are for cv_print and debugging only. Dispatch uses
   * `variant_tag`, the variant's SymbolId. Two enums that declare the same
   * variant name share a tag, so a tag only identifies a variant within one
   * enum; comparing tags across enums is meaningless. Typecheck is what keeps
   * that from happening: a value only ever reaches a match on its own enum. */
  const char *ty;
  const char *variant;
  uint32_t variant_tag;
  size_t argc;
  CieloValue *fields;
} CieloCtor;

/* `len` excludes the terminating NUL, which is always present so `data` can be
 * handed to C string functions. Runtime-built strings point `data` into the
 * tail of their own allocation; pooled literals point at static storage and
 * carry the immortal flag, so they are never freed. */
typedef struct {
  CieloArcHeader arc;
  size_t len;
  const char *data;
} CieloStr;

struct CieloValue {
  CieloTag tag;
  union {
    bool b;
    int64_t i;
    double f;
    uint32_t c;
    CieloStr *str;
    CieloCtor *ctor;
    CieloClosure *closure;
  } as;
};

/* One uniform signature for every closure body, so a call site needs to know
 * nothing about the body it dispatches to. Codegen emits one adapter per lifted
 * function to unpack these arrays into that function's own parameters. */
typedef CieloValue (*CieloClosureFn)(size_t capture_count,
                                     const CieloValue *captures, size_t argc,
                                     const CieloValue *args);

/* A function value: the lifted body plus the environment it owns. Captures are
 * copies, taken when the closure is built, and live in the tail of the same
 * block -- the same layout as CieloCtor, so destruction frees once and the ARC
 * header sits where `cielo_arc_header` expects it.
 *
 * `arity` is the body's own parameter count, excluding captures. */
struct CieloClosure {
  CieloArcHeader arc;
  CieloClosureFn call;
  /* Diagnostics only; dispatch never reads it. */
  const char *name;
  uint32_t arity;
  size_t capture_count;
  CieloValue *captures;
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

typedef struct {
  uint64_t ctor_allocations;
  uint64_t ctor_frees;
  /* Counted separately from constructors so a test can still assert the two
   * constructor totals match without strings perturbing the balance. */
  uint64_t str_allocations;
  uint64_t str_frees;
  uint64_t retain_calls;
  uint64_t release_calls;
  uint64_t release_last_calls;
} CieloArcStats;

/* One handler stack per program, not per translation unit. A unit with its own
 * copy would push a handler that no `perform` compiled elsewhere can find, and
 * the effect would escape to the no-handler trap. Capability ids have to be
 * drawn from one counter for the same reason: two units minting `1`
 * independently makes `cielo_perform_scoped` dispatch into the wrong instance.
 * These are `extern` rather than `static inline` accessors because the handler
 * stack is written on the hot path of every handler entry and exit. */
extern CieloHandlerFrame g_cielo_handlers[CIELO_HANDLER_STACK_MAX];
extern size_t g_cielo_handler_depth;
extern uint32_t g_cielo_next_capability_id;
extern CieloArcStats g_cielo_arc_stats;

#define CIELO_CALL_PURE(expr) (expr)
#define CIELO_CALL_DIRECT(expr) (expr)
#define CIELO_CALL_CONTROL(expr) (expr)

/* Counting makes every retain/release observable, so the C compiler cannot
 * fold away pairs the ARC pass already proved dead. On for tests, off for
 * benchmarks and release builds. Each unit decides for itself; the totals they
 * accumulate into are shared either way. */
#ifdef CIELO_ARC_STATS
#define CIELO_ARC_COUNT(FIELD) (g_cielo_arc_stats.FIELD++)
#else
#define CIELO_ARC_COUNT(FIELD) ((void)0)
#endif

void cielo_arc_destroy_and_dispose(CieloValue value);
bool cv_equal(CieloValue a, CieloValue b);
CieloValue cielo_make_str(const char *data, size_t len);
CieloValue cielo_make_ctor(const char *ty, const char *variant,
                           uint32_t variant_tag, size_t argc,
                           const CieloValue *fields);
CieloValue cielo_make_closure(const char *name, CieloClosureFn call,
                              uint32_t arity, size_t capture_count,
                              const CieloValue *captures);
CieloValue cielo_perform(uint32_t effect, uint32_t op_symbol, const char *op,
                         size_t argc, const CieloValue *args);
CieloValue cielo_perform_scoped(uint32_t effect,
                                uint32_t expected_capability_id,
                                uint32_t op_symbol, const char *op, size_t argc,
                                const CieloValue *args);

/* Builtins are runtime-provided operations. Codegen calls them directly for a
 * `BuiltinCall`; the dispatch table in the implementation unit only serves
 * operations that arrive through `perform`, so an effect whose name matches a
 * builtin still resolves once no handler claims it. Arguments are borrowed,
 * never released here. */
typedef CieloValue (*CieloBuiltinFn)(size_t argc, const CieloValue *args);

CieloValue cielo_builtin_print(size_t argc, const CieloValue *args);
CieloValue cielo_builtin_str_len(size_t argc, const CieloValue *args);
CieloValue cielo_builtin_str_concat(size_t argc, const CieloValue *args);

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
static inline CieloValue cv_str(CieloStr *s) { return CV_MAKE(CV_STRING, str, s); }

static inline const char *cielo_str_data(CieloValue v) {
  return v.tag == CV_STRING && v.as.str != NULL ? v.as.str->data : "";
}

static inline size_t cielo_str_len(CieloValue v) {
  return v.tag == CV_STRING && v.as.str != NULL ? v.as.str->len : 0u;
}

static inline void cielo_arc_stats_reset(void) {
  memset(&g_cielo_arc_stats, 0, sizeof(g_cielo_arc_stats));
}

static inline CieloArcStats cielo_arc_stats_snapshot(void) {
  return g_cielo_arc_stats;
}

/* Constructors and strings are both refcounted, and both put the header
 * first, so ARC only ever needs the header. NULL means unmanaged. */
static inline CieloArcHeader *cielo_arc_header(CieloValue value) {
  switch (value.tag) {
  case CV_CTOR:
    return value.as.ctor != NULL ? &value.as.ctor->arc : NULL;
  case CV_STRING:
    return value.as.str != NULL ? &value.as.str->arc : NULL;
  case CV_CLOSURE:
    return value.as.closure != NULL ? &value.as.closure->arc : NULL;
  default:
    return NULL;
  }
}

static inline bool cielo_arc_is_managed(CieloValue value) {
  return cielo_arc_header(value) != NULL;
}

static inline bool cielo_arc_is_immortal(CieloValue value) {
  const CieloArcHeader *arc = cielo_arc_header(value);
  return arc != NULL && (arc->flags & CIELO_ARC_FLAG_IMMORTAL) != 0u;
}

/* A saturated count is no longer accurate, so the object is pinned. Retain
 * and release must agree, or a saturated object gets freed early. */
static inline bool cielo_arc_header_is_pinned(const CieloArcHeader *arc) {
  return (arc->flags & CIELO_ARC_FLAG_IMMORTAL) != 0u || arc->refcount == 0u ||
         arc->refcount == UINT32_MAX;
}

static inline void cielo_arc_retain(CieloValue value) {
  CieloArcHeader *arc = cielo_arc_header(value);
  if (arc == NULL || cielo_arc_header_is_pinned(arc))
    return;
  arc->refcount++;
  CIELO_ARC_COUNT(retain_calls);
}

static inline bool cielo_arc_dec_is_last(CieloValue value) {
  CieloArcHeader *arc = cielo_arc_header(value);
  if (arc == NULL || cielo_arc_header_is_pinned(arc))
    return false;
  arc->refcount--;
  return arc->refcount == 0u;
}

static inline void cielo_arc_release(CieloValue value) {
  if (!cielo_arc_is_managed(value))
    return;
  CIELO_ARC_COUNT(release_calls);
  if (!cielo_arc_dec_is_last(value))
    return;
  CIELO_ARC_COUNT(release_last_calls);
  cielo_arc_destroy_and_dispose(value);
}

/* Hands a value on to a sink position while keeping the caller's own
 * reference. A closure's captures outlive the call, so the lifted body -- whose
 * parameters are sink arguments like any other function's -- gets a copy. */
static inline CieloValue cielo_arc_retained(CieloValue value) {
  cielo_arc_retain(value);
  return value;
}

/* Every union read goes through a checked accessor: a wrong tag here would
 * call an arbitrary address. */
static inline CieloClosure *cielo_closure_of(CieloValue value) {
  if (value.tag != CV_CLOSURE || value.as.closure == NULL)
    cielo_trap("call of a value that is not a closure");
  return value.as.closure;
}

/* Arguments are sink arguments, as at a direct call; the captures stay owned by
 * the closure, which is why the adapter retains rather than moves them. */
static inline CieloValue cielo_closure_call(CieloValue callee, size_t argc,
                                            const CieloValue *args) {
  CieloClosure *closure = cielo_closure_of(callee);
  if (closure->call == NULL)
    cielo_trap("closure has no body");
  if (closure->arity != (uint32_t)argc)
    cielo_trap("closure called with the wrong number of arguments");
  return closure->call(closure->capture_count, closure->captures, argc, args);
}

static inline bool cielo_ctor_is_variant(CieloValue value,
                                         uint32_t variant_tag) {
  return value.tag == CV_CTOR && value.as.ctor != NULL &&
         value.as.ctor->variant_tag == variant_tag;
}

static inline CieloValue cielo_ctor_field(CieloValue value, size_t index) {
  if (value.tag != CV_CTOR || value.as.ctor == NULL)
    return cv_unit();
  if (index >= value.as.ctor->argc || value.as.ctor->fields == NULL)
    return cv_unit();
  return value.as.ctor->fields[index];
}

/* Reading a field hands back a retained reference, so the projection stays
 * valid independently of the parent. ARC owns and releases the result. */
static inline CieloValue cielo_ctor_field_copy(CieloValue value, size_t index) {
  CieloValue field = cielo_ctor_field(value, index);
  cielo_arc_retain(field);
  return field;
}

/* A wrong compile-time uniqueness answer corrupts another holder's field and
 * shows up nowhere near the mistake. The stats build is the test build, so it
 * re-checks the proof it was handed. */
#ifdef CIELO_ARC_STATS
#define CIELO_ARC_ASSERT_UNIQUE(CTOR)                                          \
  do {                                                                         \
    if (((CTOR)->arc.flags & CIELO_ARC_FLAG_IMMORTAL) != 0u ||                 \
        (CTOR)->arc.refcount != 1u)                                            \
      cielo_trap("statically unique take on a shared constructor");            \
  } while (0)
#else
#define CIELO_ARC_ASSERT_UNIQUE(CTOR) ((void)0)
#endif

/* Move a field out of a constructor the caller is the sole owner of. Clearing
 * the slot is Nim's `wasMoved`: destroying the parent no longer decrements the
 * transferred field. Emitted only for `CfgProjectionMode::MoveUnique`, where
 * the uniqueness query proved the ownership the runtime otherwise tests for. */
static inline CieloValue cielo_ctor_take_field_unique(CieloValue value,
                                                      size_t index) {
  if (value.tag != CV_CTOR || value.as.ctor == NULL)
    return cv_unit();
  CieloCtor *ctor = value.as.ctor;
  if (index >= ctor->argc || ctor->fields == NULL)
    return cv_unit();
  CIELO_ARC_ASSERT_UNIQUE(ctor);
  CieloValue result = ctor->fields[index];
  ctor->fields[index] = cv_unit();
  return result;
}

/* The ARC pass picks Move from intraprocedural liveness, which proves the
 * parent is dead here, not that it is unique. Clearing a shared or pooled
 * parent would corrupt the other holders, so check uniqueness first and fall
 * back to a borrow. */
static inline CieloValue cielo_ctor_take_field(CieloValue value, size_t index) {
  if (value.tag != CV_CTOR || value.as.ctor == NULL)
    return cv_unit();
  CieloCtor *ctor = value.as.ctor;
  if (index >= ctor->argc || ctor->fields == NULL)
    return cv_unit();
  if (cielo_arc_is_immortal(value) || ctor->arc.refcount != 1u) {
    CieloValue result = ctor->fields[index];
    cielo_arc_retain(result);
    return result;
  }
  return cielo_ctor_take_field_unique(value, index);
}

/* One allocation substrate for everything a lexical scope owns: handler
 * evidence today, continuation and closure environments later. A region is a
 * bump arena freed whole at close, so a slot needs no individual free and no
 * refcount -- nothing can observe it after the close.
 *
 * The escape analysis places slots it proves confined directly in the C frame
 * and never opens a region for them at all; this path exists for the slots it
 * cannot prove, where a plain automatic would dangle. */
typedef struct CieloRegionChunk {
  struct CieloRegionChunk *next;
  size_t used;
  size_t capacity;
  /* Over-aligned so a chunk can back any slot type without the bump pointer
   * having to know what will land in it. */
  _Alignas(max_align_t) unsigned char data[];
} CieloRegionChunk;

typedef struct {
  CieloRegionChunk *head;
} CieloRegion;

enum { CIELO_REGION_CHUNK_MIN = 512 };

static inline void cielo_region_close(CieloRegion *region);

/* Reclaims first rather than just clearing the head: a region opened inside a
 * loop re-enters this with the previous generation's chunks still attached, and
 * dropping the pointer there would leak them. */
static inline void cielo_region_open(CieloRegion *region) {
  cielo_region_close(region);
}

static inline void *cielo_region_alloc(CieloRegion *region, size_t size) {
  size_t aligned = (size + (_Alignof(max_align_t) - 1u)) &
                   ~(size_t)(_Alignof(max_align_t) - 1u);
  if (region->head == NULL ||
      region->head->capacity - region->head->used < aligned) {
    size_t capacity = aligned > CIELO_REGION_CHUNK_MIN ? aligned
                                                       : CIELO_REGION_CHUNK_MIN;
    CieloRegionChunk *chunk =
        (CieloRegionChunk *)malloc(sizeof(CieloRegionChunk) + capacity);
    if (chunk == NULL)
      cielo_trap("out of memory allocating a region chunk");
    chunk->next = region->head;
    chunk->used = 0;
    chunk->capacity = capacity;
    region->head = chunk;
  }
  void *slot = region->head->data + region->head->used;
  region->head->used += aligned;
  return slot;
}

/* Idempotent: a close on an already-closed region is a no-op, so a region left
 * open on a path the analysis rejected still frees exactly once. */
static inline void cielo_region_close(CieloRegion *region) {
  CieloRegionChunk *chunk = region->head;
  region->head = NULL;
  while (chunk != NULL) {
    CieloRegionChunk *next = chunk->next;
    free(chunk);
    chunk = next;
  }
}

static inline uint32_t cielo_handler_push(uint32_t effect) {
  if (g_cielo_handler_depth >= CIELO_HANDLER_STACK_MAX)
    cielo_trap("handler stack overflow");
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

/* Unwinds to and including the named frame. An id that is not on the stack
 * would otherwise unwind to zero, discharging every enclosing handler. */
static inline void cielo_handler_pop(uint32_t capability_id) {
  if (capability_id == 0)
    return;
  size_t depth = g_cielo_handler_depth;
  while (depth > 0) {
    depth--;
    if (g_cielo_handlers[depth].capability_id == capability_id) {
      /* Evidence can live in a region that is freed right after this pop.
       * Leaving the pointer behind would keep a dangling address in a global,
       * which is both a hazard and enough to make LeakSanitizer treat a leaked
       * region chunk as still reachable. */
      for (size_t i = depth; i < g_cielo_handler_depth; i++) {
        g_cielo_handlers[i].evidence = NULL;
        g_cielo_handlers[i].capability_id = 0;
      }
      g_cielo_handler_depth = depth;
      return;
    }
  }
  cielo_trap("handler pop for a capability that is not on the stack");
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

/* Returns int64_t, not int: narrowing here would alias a selector above
 * INT_MAX onto a live case label. */
static inline int64_t cv_switch_index(CieloValue v) {
  if (v.tag != CV_INT)
    cielo_trap("switch selector is not an integer");
  return v.as.i;
}

/* Arithmetic dispatches on the tag. Reading `.as.i` unconditionally treats a
 * double's bit pattern as an integer, which is why CV_FLOAT could never be
 * computed with. The int case is first because it is the only one the current
 * frontend can produce. */
static inline bool cv_both(CieloValue a, CieloValue b, CieloTag tag) {
  return a.tag == tag && b.tag == tag;
}

/* Signed integer overflow is undefined in C, so the compiler is entitled to
 * assume it never happens and miscompile the surrounding code -- wrapping is
 * not a safe fallback. Every int arithmetic path therefore checks first and
 * traps, mirroring cv_div. Float paths are exempt: IEEE 754 gives infinity,
 * which is defined. Requires GCC 5+ or Clang for __builtin_*_overflow; the
 * driver emits for gcc. */
static inline CieloValue cv_neg(CieloValue a) {
  if (a.tag == CV_FLOAT)
    return cv_float(-a.as.f);
  if (a.tag != CV_INT)
    cielo_trap("negation of a non-numeric value");
  int64_t out;
  if (__builtin_sub_overflow((int64_t)0, a.as.i, &out))
    cielo_trap("integer negation overflow");
  return cv_int(out);
}
static inline CieloValue cv_not(CieloValue a) { return cv_bool(!cv_truthy(a)); }
static inline CieloValue cv_add(CieloValue a, CieloValue b) {
  if (cv_both(a, b, CV_INT)) {
    int64_t out;
    if (__builtin_add_overflow(a.as.i, b.as.i, &out))
      cielo_trap("integer addition overflow");
    return cv_int(out);
  }
  if (cv_both(a, b, CV_FLOAT))
    return cv_float(a.as.f + b.as.f);
  cielo_trap("addition of non-numeric or mismatched operands");
}
static inline CieloValue cv_sub(CieloValue a, CieloValue b) {
  if (cv_both(a, b, CV_INT)) {
    int64_t out;
    if (__builtin_sub_overflow(a.as.i, b.as.i, &out))
      cielo_trap("integer subtraction overflow");
    return cv_int(out);
  }
  if (cv_both(a, b, CV_FLOAT))
    return cv_float(a.as.f - b.as.f);
  cielo_trap("subtraction of non-numeric or mismatched operands");
}
static inline CieloValue cv_mul(CieloValue a, CieloValue b) {
  if (cv_both(a, b, CV_INT)) {
    int64_t out;
    if (__builtin_mul_overflow(a.as.i, b.as.i, &out))
      cielo_trap("integer multiplication overflow");
    return cv_int(out);
  }
  if (cv_both(a, b, CV_FLOAT))
    return cv_float(a.as.f * b.as.f);
  cielo_trap("multiplication of non-numeric or mismatched operands");
}
static inline CieloValue cv_div(CieloValue a, CieloValue b) {
  /* IEEE division by zero is defined, so only the integer path traps. */
  if (cv_both(a, b, CV_FLOAT))
    return cv_float(a.as.f / b.as.f);
  if (!cv_both(a, b, CV_INT))
    cielo_trap("division of non-numeric or mismatched operands");
  if (b.as.i == 0)
    cielo_trap("divide by zero");
  if (a.as.i == INT64_MIN && b.as.i == -1)
    cielo_trap("integer division overflow");
  return cv_int(a.as.i / b.as.i);
}
static inline CieloValue cv_mod(CieloValue a, CieloValue b) {
  if (!cv_both(a, b, CV_INT))
    cielo_trap("modulo of non-integer or mismatched operands");
  if (b.as.i == 0)
    cielo_trap("modulo by zero");
  if (a.as.i == INT64_MIN && b.as.i == -1)
    return cv_int(0);
  return cv_int(a.as.i % b.as.i);
}
static inline const char *cielo_cstr0(const char *s) { return s ? s : ""; }

static inline CieloValue cv_eq(CieloValue a, CieloValue b) {
  return cv_bool(cv_equal(a, b));
}
static inline CieloValue cv_ne(CieloValue a, CieloValue b) {
  return cv_bool(!cv_equal(a, b));
}

/* Orders values of the same tag. Mixed tags are a type error the frontend
 * should have rejected. */
static inline int cv_ordering(CieloValue a, CieloValue b) {
  if (a.tag != b.tag)
    cielo_trap("ordering comparison between different types");
  switch (a.tag) {
  case CV_INT:
    return a.as.i < b.as.i ? -1 : (a.as.i > b.as.i ? 1 : 0);
  case CV_FLOAT:
    return a.as.f < b.as.f ? -1 : (a.as.f > b.as.f ? 1 : 0);
  case CV_CHAR:
    return a.as.c < b.as.c ? -1 : (a.as.c > b.as.c ? 1 : 0);
  case CV_BOOL:
    return (int)a.as.b - (int)b.as.b;
  case CV_STRING: {
    int r = strcmp(cielo_str_data(a), cielo_str_data(b));
    return r < 0 ? -1 : (r > 0 ? 1 : 0);
  }
  case CV_UNIT:
    return 0;
  case CV_CTOR:
  case CV_CLOSURE:
    break;
  }
  cielo_trap("ordering comparison on a constructor or closure value");
}

static inline CieloValue cv_lt(CieloValue a, CieloValue b) {
  return cv_bool(cv_ordering(a, b) < 0);
}
static inline CieloValue cv_le(CieloValue a, CieloValue b) {
  return cv_bool(cv_ordering(a, b) <= 0);
}
static inline CieloValue cv_gt(CieloValue a, CieloValue b) {
  return cv_bool(cv_ordering(a, b) > 0);
}
static inline CieloValue cv_ge(CieloValue a, CieloValue b) {
  return cv_bool(cv_ordering(a, b) >= 0);
}
static inline CieloValue cv_and(CieloValue a, CieloValue b) {
  return cv_bool(cv_truthy(a) && cv_truthy(b));
}
static inline CieloValue cv_or(CieloValue a, CieloValue b) {
  return cv_bool(cv_truthy(a) || cv_truthy(b));
}

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
    CASE_PUTS(CV_STRING, cielo_str_data(v));

  case CV_CLOSURE:
    printf("<closure %s>\n",
           v.as.closure ? cielo_cstr0(v.as.closure->name) : "?");
    break;

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

#ifdef CIELO_RUNTIME_IMPL

CieloHandlerFrame g_cielo_handlers[CIELO_HANDLER_STACK_MAX];
size_t g_cielo_handler_depth = 0;
uint32_t g_cielo_next_capability_id = 1;
CieloArcStats g_cielo_arc_stats = {0};

_Noreturn void cielo_trap(const char *what) {
  fflush(stdout);
  fprintf(stderr, "cielo: %s\n", what);
  abort();
}

_Noreturn void cielo_trap_op(const char *what, const char *op) {
  fflush(stdout);
  fprintf(stderr, "cielo: %s: %s\n", what, op ? op : "?");
  abort();
}

/* Destruction uses an explicit worklist. Recursing would overflow the C
 * stack on any structure as deep as its input is long. */
typedef struct {
  CieloValue *items;
  size_t len;
  size_t cap;
} CieloDropStack;

static void cielo_drop_stack_push(CieloDropStack *stack, CieloValue value) {
  if (stack->len == stack->cap) {
    size_t cap = stack->cap ? stack->cap * 2u : 16u;
    CieloValue *items =
        (CieloValue *)realloc(stack->items, cap * sizeof(CieloValue));
    if (items == NULL)
      cielo_trap("out of memory growing drop stack");
    stack->items = items;
    stack->cap = cap;
  }
  stack->items[stack->len++] = value;
}

void cielo_arc_destroy_and_dispose(CieloValue value) {
  if (!cielo_arc_is_managed(value) || cielo_arc_is_immortal(value))
    return;

  CieloDropStack stack = {NULL, 0u, 0u};
  cielo_drop_stack_push(&stack, value);

  while (stack.len > 0u) {
    CieloValue dying = stack.items[--stack.len];

    /* Strings own no children, and their bytes live in the tail of the same
     * block, so one free finishes them. */
    if (dying.tag == CV_STRING) {
      CIELO_ARC_COUNT(str_frees);
      free(dying.as.str);
      continue;
    }

    /* Counted with constructors rather than in a pair of its own, so the
     * allocation balance a test asserts covers closures too. */
    if (dying.tag == CV_CLOSURE) {
      CieloClosure *closure = dying.as.closure;
      CieloValue *captures = closure->captures;
      size_t capture_count = closure->capture_count;
      closure->captures = NULL;
      closure->capture_count = 0u;
      for (size_t i = 0; i < capture_count; i++) {
        CieloValue capture = captures[i];
        if (!cielo_arc_is_managed(capture))
          continue;
        CIELO_ARC_COUNT(release_calls);
        if (!cielo_arc_dec_is_last(capture))
          continue;
        CIELO_ARC_COUNT(release_last_calls);
        cielo_drop_stack_push(&stack, capture);
      }
      CIELO_ARC_COUNT(ctor_frees);
      free(closure);
      continue;
    }

    CieloCtor *ctor = dying.as.ctor;
    CieloValue *fields = ctor->fields;
    size_t argc = ctor->argc;
    ctor->fields = NULL;
    ctor->argc = 0u;

    for (size_t i = 0; i < argc; i++) {
      CieloValue field = fields[i];
      if (!cielo_arc_is_managed(field))
        continue;
      CIELO_ARC_COUNT(release_calls);
      if (!cielo_arc_dec_is_last(field))
        continue;
      CIELO_ARC_COUNT(release_last_calls);
      cielo_drop_stack_push(&stack, field);
    }

    /* `fields` points into the tail of `ctor`; one free covers both. */
    CIELO_ARC_COUNT(ctor_frees);
    free(ctor);
  }

  free(stack.items);
}

bool cv_equal(CieloValue a, CieloValue b) {
  if (a.tag != b.tag)
    return false;
  switch (a.tag) {
  case CV_UNIT:
    return true;
  case CV_BOOL:
    return a.as.b == b.as.b;
  case CV_INT:
    return a.as.i == b.as.i;
  case CV_FLOAT:
    return a.as.f == b.as.f;
  case CV_CHAR:
    return a.as.c == b.as.c;
  case CV_STRING: {
    /* By value, never by pointer: two equal strings built at runtime are
     * distinct allocations, and pooling only dedups literals. */
    size_t len = cielo_str_len(a);
    return len == cielo_str_len(b) &&
           memcmp(cielo_str_data(a), cielo_str_data(b), len) == 0;
  }
  /* Function values have no structural equality, so identity is the only
   * answer that is not a guess. */
  case CV_CLOSURE:
    return a.as.closure == b.as.closure;
  case CV_CTOR:
    break;
  }
  /* Constructors compare structurally. Values are immutable and built
   * bottom-up, so the heap is a DAG and this terminates. */
  const CieloCtor *x = a.as.ctor;
  const CieloCtor *y = b.as.ctor;
  if (x == y)
    return true;
  if (x == NULL || y == NULL)
    return false;
  if (x->argc != y->argc || x->variant_tag != y->variant_tag)
    return false;
  for (size_t i = 0; i < x->argc; i++) {
    if (!cv_equal(x->fields[i], y->fields[i]))
      return false;
  }
  return true;
}

/* Bytes live in the tail of the same block, so destruction frees once. The
 * result is owned: the caller's ARC releases it. */
CieloValue cielo_make_str(const char *data, size_t len) {
  CieloStr *str = (CieloStr *)malloc(sizeof(CieloStr) + len + 1u);
  if (str == NULL)
    cielo_trap("out of memory allocating string");
  char *bytes = (char *)(str + 1);
  if (len > 0u && data != NULL)
    memcpy(bytes, data, len);
  bytes[len] = '\0';
  str->arc = (CieloArcHeader)CIELO_ARC_OWNED_HEADER;
  str->len = len;
  str->data = bytes;
  CIELO_ARC_COUNT(str_allocations);
  return cv_str(str);
}

/* Arguments are sink arguments, so every builtin ends by releasing them. The
 * caller retains beforehand only when the value stays live. */
static void cielo_builtin_release_args(size_t argc, const CieloValue *args) {
  for (size_t i = 0; i < argc; i++) {
    cielo_arc_release(args[i]);
  }
}

CieloValue cielo_builtin_print(size_t argc, const CieloValue *args) {
  if (argc != 1 || args == NULL)
    cielo_trap("print expects exactly one argument");
  cv_print(args[0]);
  cielo_builtin_release_args(argc, args);
  return cv_unit();
}

CieloValue cielo_builtin_str_len(size_t argc, const CieloValue *args) {
  if (argc != 1 || args == NULL)
    cielo_trap("str_len expects exactly one argument");
  if (args[0].tag != CV_STRING)
    cielo_trap("str_len expects a string");
  CieloValue out = cv_int((int64_t)cielo_str_len(args[0]));
  cielo_builtin_release_args(argc, args);
  return out;
}

CieloValue cielo_builtin_str_concat(size_t argc, const CieloValue *args) {
  if (argc != 2 || args == NULL)
    cielo_trap("str_concat expects exactly two arguments");
  if (args[0].tag != CV_STRING || args[1].tag != CV_STRING)
    cielo_trap("str_concat expects strings");
  size_t left = cielo_str_len(args[0]);
  size_t right = cielo_str_len(args[1]);
  if (left > SIZE_MAX - right - 1u)
    cielo_trap("string concatenation length overflow");
  CieloValue out = cielo_make_str(NULL, left + right);
  char *bytes = (char *)out.as.str->data;
  memcpy(bytes, cielo_str_data(args[0]), left);
  memcpy(bytes + left, cielo_str_data(args[1]), right);
  /* Copies are done, so releasing here cannot free bytes still being read. */
  cielo_builtin_release_args(argc, args);
  return out;
}

typedef struct {
  uint32_t op_symbol;
  CieloBuiltinFn fn;
} CieloBuiltinEntry;

#define CIELO_BUILTIN_ENTRY(SYMBOL, FN) {(SYMBOL), (FN)},

/* SymbolIds are only stable within a compilation unit, so the keys cannot be
 * baked into this header; codegen defines CIELO_BUILTIN_TABLE above it. Read
 * only, so one copy is a link-time convenience rather than a correctness
 * requirement -- but it lives here because `cielo_perform` does, and a unit
 * with a private empty table would trap on a builtin the program does define. */
static const CieloBuiltinEntry g_cielo_builtins[] = {
#ifdef CIELO_BUILTIN_TABLE
    CIELO_BUILTIN_TABLE(CIELO_BUILTIN_ENTRY)
#endif
        {0u, NULL}};

static CieloBuiltinFn cielo_builtin_lookup(uint32_t op_symbol) {
  if (op_symbol == 0u)
    return NULL;
  for (size_t i = 0; g_cielo_builtins[i].fn != NULL; i++) {
    if (g_cielo_builtins[i].op_symbol == op_symbol)
      return g_cielo_builtins[i].fn;
  }
  return NULL;
}

CieloValue cielo_perform(uint32_t effect, uint32_t op_symbol, const char *op,
                         size_t argc, const CieloValue *args) {
  return cielo_perform_scoped(effect, 0, op_symbol, op, argc, args);
}

CieloValue cielo_perform_scoped(uint32_t effect,
                                uint32_t expected_capability_id,
                                uint32_t op_symbol, const char *op, size_t argc,
                                const CieloValue *args) {
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
        cielo_trap("handler has no clause for this operation");
      }
    }
    // Falling back to another handler instance with the same effect label
    // would run the wrong clause, and returning unit is indistinguishable
    // from a real unit result.
    cielo_trap_op("scoped handler is no longer in scope", op);
  }
  for (size_t i = g_cielo_handler_depth; i > 0; i--) {
    if (g_cielo_handlers[i - 1].effect == effect) {
      CieloEvidence *evidence = g_cielo_handlers[i - 1].evidence;
      if (cielo_dispatch_with_evidence(evidence, op_symbol, argc, args,
                                       &dispatched)) {
        return dispatched;
      }
      cielo_trap("handler has no clause for this operation");
    }
  }
  CieloBuiltinFn builtin = cielo_builtin_lookup(op_symbol);
  if (builtin != NULL)
    return builtin(argc, args);
  /* An effect escaped every handler. Returning unit would make that
   * indistinguishable from a handler that ran and produced unit. */
  cielo_trap_op("effect performed with no handler in scope", op);
}

/* One allocation per constructor: fields live in the tail of the same block.
 * CieloCtor and CieloValue share the platform's max scalar alignment, so
 * `ctor + 1` is aligned.
 *
 * Pooled constructors keep their own static field arrays, but they are
 * immortal and never destroyed, so destruction can always assume fields are
 * inline and free the block once. */
CieloValue cielo_make_ctor(const char *ty, const char *variant,
                           uint32_t variant_tag, size_t argc,
                           const CieloValue *fields) {
  CieloCtor *ctor =
      (CieloCtor *)malloc(sizeof(CieloCtor) + argc * sizeof(CieloValue));
  if (ctor == NULL) {
    cielo_trap("out of memory allocating constructor");
  }

  ctor->arc = (CieloArcHeader)CIELO_ARC_OWNED_HEADER;
  ctor->ty = ty;
  ctor->variant = variant;
  ctor->variant_tag = variant_tag;
  ctor->argc = argc;
  ctor->fields = argc > 0 ? (CieloValue *)(ctor + 1) : NULL;

  if (argc > 0) {
    if (fields != NULL) {
      /* Constructor arguments are sink arguments. The generated CFG inserts a
       * retain only for fields that remain live at the call site. */
      memcpy(ctor->fields, fields, sizeof(CieloValue) * argc);
    } else {
      for (size_t i = 0; i < argc; i++) {
        ctor->fields[i] = cv_unit();
      }
    }
  }

  CIELO_ARC_COUNT(ctor_allocations);
  CieloValue out = {.tag = CV_CTOR};
  out.as.ctor = ctor;
  return out;
}

/* Captures live in the tail of the same block, like a constructor's fields, and
 * arrive as sink arguments: the closure takes over the references its builder
 * held. */
CieloValue cielo_make_closure(const char *name, CieloClosureFn call,
                              uint32_t arity, size_t capture_count,
                              const CieloValue *captures) {
  CieloClosure *closure = (CieloClosure *)malloc(sizeof(CieloClosure) +
                                                 capture_count *
                                                     sizeof(CieloValue));
  if (closure == NULL)
    cielo_trap("out of memory allocating closure");

  closure->arc = (CieloArcHeader)CIELO_ARC_OWNED_HEADER;
  closure->call = call;
  closure->name = name;
  closure->arity = arity;
  closure->capture_count = capture_count;
  closure->captures =
      capture_count > 0 ? (CieloValue *)(closure + 1) : NULL;
  if (capture_count > 0 && captures != NULL)
    memcpy(closure->captures, captures, sizeof(CieloValue) * capture_count);

  CIELO_ARC_COUNT(ctor_allocations);
  CieloValue out = {.tag = CV_CLOSURE};
  out.as.closure = closure;
  return out;
}

#endif /* CIELO_RUNTIME_IMPL */

#endif /* CIELO_RUNTIME_H */
