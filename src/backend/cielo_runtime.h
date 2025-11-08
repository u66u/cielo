#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef enum {
    CV_UNIT = 0,
    CV_BOOL = 1,
    CV_INT = 2,
    CV_FLOAT = 3,
    CV_CHAR = 4,
    CV_STRING = 5
} CieloTag;

typedef struct {
    CieloTag tag;
    union {
        bool b;
        int64_t i;
        double f;
        uint32_t c;
        const char* s;
    } as;
} CieloValue;

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
        default:
            printf("<value>\n");
            break;
    }
}

static CieloValue cielo_perform(
    uint32_t effect, const char* op, size_t argc, const CieloValue* args
) {
    (void)effect;
    if (op && strcmp(op, "print") == 0 && argc > 0 && args != NULL) {
        cv_print(args[0]);
        return cv_unit();
    }
    return cv_unit();
}

static CieloValue cielo_make_ctor(
    const char* ty, const char* variant, size_t argc, const CieloValue* fields
) {
    (void)ty;
    (void)variant;
    (void)argc;
    (void)fields;
    return cv_unit();
}
