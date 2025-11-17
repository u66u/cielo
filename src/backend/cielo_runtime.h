#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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

typedef struct {
    const char* ty;
    const char* variant;
    size_t argc;
    CieloValue* fields;
} CieloCtor;

struct CieloValue {
    CieloTag tag;
    union {
        bool b;
        int64_t i;
        double f;
        uint32_t c;
        const char* s;
        CieloCtor* ctor;
    } as;
};

typedef struct {
    uint32_t effect;
} CieloHandlerFrame;

enum { CIELO_HANDLER_STACK_MAX = 64 };
static CieloHandlerFrame g_cielo_handlers[CIELO_HANDLER_STACK_MAX];
static size_t g_cielo_handler_depth = 0;

#define CIELO_CALL_PURE(expr) (expr)
#define CIELO_CALL_DIRECT(expr) (expr)
#define CIELO_CALL_CONTROL(expr) (expr)

static inline CieloValue cv_unit(void) {
    CieloValue v = {.tag = CV_UNIT};
    return v;
}
static inline CieloValue cv_bool(int x) {
    CieloValue v = {.tag = CV_BOOL};
    v.as.b = x != 0;
    return v;
}
static inline CieloValue cv_int(int64_t x) {
    CieloValue v = {.tag = CV_INT};
    v.as.i = x;
    return v;
}
static inline CieloValue cv_float(double x) {
    CieloValue v = {.tag = CV_FLOAT};
    v.as.f = x;
    return v;
}
static inline CieloValue cv_char(uint32_t x) {
    CieloValue v = {.tag = CV_CHAR};
    v.as.c = x;
    return v;
}
static inline CieloValue cv_string(const char* s) {
    CieloValue v = {.tag = CV_STRING};
    v.as.s = s;
    return v;
}

static inline bool cielo_ctor_is_variant(CieloValue value, const char* variant) {
    if (value.tag != CV_CTOR || value.as.ctor == NULL) return false;
    if (value.as.ctor->variant == NULL || variant == NULL) return false;
    return strcmp(value.as.ctor->variant, variant) == 0;
}

static inline CieloValue cielo_ctor_field(CieloValue value, size_t index) {
    if (value.tag != CV_CTOR || value.as.ctor == NULL) return cv_unit();
    if (index >= value.as.ctor->argc || value.as.ctor->fields == NULL) return cv_unit();
    return value.as.ctor->fields[index];
}

static inline void cielo_handler_push(uint32_t effect) {
    if (g_cielo_handler_depth >= CIELO_HANDLER_STACK_MAX) return;
    g_cielo_handlers[g_cielo_handler_depth].effect = effect;
    g_cielo_handler_depth++;
}

static inline void cielo_handler_pop(uint32_t effect) {
    while (g_cielo_handler_depth > 0) {
        g_cielo_handler_depth--;
        if (g_cielo_handlers[g_cielo_handler_depth].effect == effect) return;
    }
}

static inline bool cielo_handler_active(uint32_t effect) {
    for (size_t i = g_cielo_handler_depth; i > 0; i--) {
        if (g_cielo_handlers[i - 1].effect == effect) return true;
    }
    return false;
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
    if (a.tag != b.tag) return cv_bool(0);
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

static inline void cv_print(CieloValue v) {
    switch (v.tag) {
        case CV_UNIT:
            printf("()\n");
            break;
        case CV_BOOL:
            printf("%s\n", v.as.b ? "true" : "false");
            break;
        case CV_INT:
            printf("%lld\n", (long long)v.as.i);
            break;
        case CV_FLOAT:
            printf("%f\n", v.as.f);
            break;
        case CV_CHAR:
            printf("%c\n", (int)v.as.c);
            break;
        case CV_STRING:
            printf("%s\n", v.as.s ? v.as.s : "");
            break;
        case CV_CTOR:
            if (v.as.ctor != NULL) {
                printf(
                    "<%s.%s/%zu>\n",
                    v.as.ctor->ty ? v.as.ctor->ty : "",
                    v.as.ctor->variant ? v.as.ctor->variant : "",
                    v.as.ctor->argc
                );
            } else {
                printf("<ctor>\n");
            }
            break;
        default:
            printf("<value>\n");
            break;
    }
}

static CieloValue cielo_perform(
    uint32_t effect, const char* op, size_t argc, const CieloValue* args
) {
    if (cielo_handler_active(effect)) {
        return cv_unit();
    }
    if (op && strcmp(op, "print") == 0 && argc > 0 && args != NULL) {
        cv_print(args[0]);
        return cv_unit();
    }
    return cv_unit();
}

static CieloValue cielo_make_ctor(
    const char* ty, const char* variant, size_t argc, const CieloValue* fields
) {
    CieloCtor* ctor = (CieloCtor*)malloc(sizeof(CieloCtor));
    if (ctor == NULL) return cv_unit();

    ctor->ty = ty;
    ctor->variant = variant;
    ctor->argc = argc;
    ctor->fields = NULL;

    if (argc > 0) {
        ctor->fields = (CieloValue*)malloc(sizeof(CieloValue) * argc);
        if (ctor->fields == NULL) {
            free(ctor);
            return cv_unit();
        }
        if (fields != NULL) {
            memcpy(ctor->fields, fields, sizeof(CieloValue) * argc);
        } else {
            for (size_t i = 0; i < argc; i++) {
                ctor->fields[i] = cv_unit();
            }
        }
    }

    CieloValue out = {.tag = CV_CTOR};
    out.as.ctor = ctor;
    return out;
}
