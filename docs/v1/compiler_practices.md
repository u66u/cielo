### Memory & Data Layout

**1. Arena Allocation**

```C
static void* arena_alloc(size_t size) {
    size = (size + 7) & ~7;
    void* p = Arena.data + Arena.used;
    Arena.used += size;
    return p;
}
```

_One contiguous block, bump pointer, no individual frees. Eliminates memory management complexity entirely. Compiler lifetime = arena lifetime._

**2. String Interning**

```C
static StringId intern(const char* s) { /* hash lookup, dedup */ }
static const char* str(StringId id) { return Strings.strings[id].data; }
```

_All strings become 32-bit IDs. Comparison is integer equality. Hashing done once at intern time. Reduces memory, speeds up symbol table operations._

**3. ID-Based References (Everything is an Index)**

```C
typedef uint32_t TypeId;
typedef uint32_t FuncId;
typedef uint32_t RegId;
```

_No pointers between compiler data structures—only indices into global arrays. Makes serialization trivial, enables easy debugging (print ID numbers), and keeps cache locality._

---

### Parsing & Frontend

**6. Pratt Parsing for Expressions**

```C
static RegId parse_binary(Parser* p, Prec min_prec) {
    RegId left = parse_unary(p);
    while (get_prec(p->cur.kind) >= min_prec) {
        TokKind op = p->cur.kind;
        parser_advance(p);
        RegId right = parse_binary(p, get_prec(op) + 1);
        left = ir_binop(b, tok_to_op(op), ...);
    }
    return left;
}
```

_Handles operator precedence in ~30 lines. No grammar specification needed. Easily extensible—add new operators by adding precedence entries._

**9. Scope as Linear Array with Markers**

C

```
VarEntry vars[256];
uint32_t scope_starts[32];  // Index into vars[] at each scope level
```

_Push scope = save var_count. Pop scope = restore var_count. No hash table per scope. Variable lookup is linear but scopes are small—fast enough._

---

### Type System

**10. Union-Find for Type Unification**

C

```
static TypeId type_find(TypeId id) {
    Type* t = type_get(id);
    if (t->kind == TY_UNKNOWN && t->ref != id) {
        t->ref = type_find(t->ref);  // Path compression
        return t->ref;
    }
    return id;
}
```

_Classic algorithm for equivalence classes. O(α(n)) amortized. Type variables point to their resolved type. Path compression keeps chains short._

**11. Constraint Collection (Deferred Solving)**

C

```
// During parsing:
constraint_eq(left_type, right_type, line);

// After parsing:
solve_constraints();  // Fixed-point loop
```

_Decouple constraint generation from solving. Parse entire program, collect all constraints, then solve. Enables better error messages and handles forward references naturally._

**12. Monomorphization via Queue**

C

```
static FuncId queue_mono(FuncId generic_id, TypeId* type_args, uint32_t count) {
    // Check if already instantiated
    // If not, create placeholder, add to queue
    // Return instantiated FuncId
}
static void process_mono_queue(void) {
    while (processed < count) instantiate_func(&reqs[processed++]);
}
```

_Demand-driven instantiation. Generic functions stay generic until called with concrete types. Queue ensures transitive instantiation (generic calling generic). Simple and complete._

**13. Recursive Type Substitution**

C

```
static TypeId substitute_type(TypeId type_id, TypeId* params, TypeId* args, uint32_t count) {
    switch (t->kind) {
        case TY_GENERIC: return args[t->generic.index];
        case TY_PTR: return type_ptr(substitute_type(t->elem, ...));
        // ... recurse into all compound types
    }
}
```

_Handles nested generics correctly. `List<Option<T>>` with `T=Int` → `List<Option<Int>>`. The recursion naturally handles arbitrary nesting depth._

---

### IR Design

**14. ANF-Style Flat IR**

C

```
// Every operation has explicit destination:
let t0 = call g(x)     // OP_CALL dst=t0
let t1 = add t0, 1     // OP_ADD dst=t1, src1=t0, src2=const
let t2 = call f(t1)    // OP_CALL dst=t2
```

_No nested expressions. Every intermediate has a name (register). Makes evaluation order explicit. Codegen becomes trivial linear traversal._

**15. Block Arguments (Phi-Replaced)**

text

```
block merge(x: i32):     // Parameter, not phi
    return x

jump merge(value)        // Argument passed explicitly
```

_Unifies function calls, jumps, and loop back-edges. The "argument" tells you exactly which value flows to which parameter. SSA without the complexity of phi placement algorithms._

**16. IR Builder Pattern**

C

```
static RegId ir_const_int(IRBuilder* b, int64_t val) {
    RegId r = ir_new_reg(b, Types.t_int);
    ir_emit(b, (Inst){.op = OP_CONST_INT, .dst = r, .imm_int = val});
    return r;
}
```

_Hide instruction creation behind helpers. Call site reads naturally: `ir_binop(b, OP_ADD, type, left, right)`. Reduces errors, makes refactoring easier._


---

### Analysis & Optimization

**18. Escape Analysis via Backward Fixed-Point**

C

```
static void analyze_escapes(FuncIR* f) {
    // Initialize all non-escaping
    // Mark returns, call args, captured values as escaping
    // Propagate through moves until fixpoint
}
```

_Simple iterative algorithm. Useful for stack allocation decisions. Could feed into ownership/borrow analysis. Fixed-point loop handles complex control flow._


**20. Lambda Lifting (Closures → Functions + Env)**

C

```
// Lambda becomes:
// 1. New function with __env parameter
// 2. Struct holding captured values
// 3. OP_CLOSURE_NEW at call site
```

_Eliminates first-class closures from IR. Every "closure" is a function pointer + environment pointer. C codegen straightforward: `func(__env, args...)`._

---



### Code Generation

**23. Direct C Emission**

C

```
case OP_ADD:
    emit("    _r%u = _r%u + _r%u;\n", inst->dst, inst->src1, inst->src2);
    break;
```

_No intermediate representation before C. Pattern match on opcodes, emit strings. The C compiler handles register allocation, instruction selection, optimization._

**24. On-Demand Type Emission**

C

```
static StringId get_array_typedef_name(TypeId elem_type) {
    // Check if already emitted
    // If not, create "Array_Int" style name, record it
    return name;
}
```

_Don't pre-declare all types. Emit them as encountered during codegen. Handles monomorphized types naturally—they're created during instantiation and emitted when used._

**25. Labels as Goto Targets**

C

```
case OP_LABEL: emit("_L%u:;\n", inst->label); break;
case OP_JUMP:  emit("    goto _L%u;\n", inst->label); break;
case OP_BRANCH: emit("    if (_r%u) goto _L%u; else goto _L%u;\n", ...);
```


**30. First-Class Error Nodes**

C

```
struct Node {
    NodeKind kind;  // Can be ERROR
    Span span;
    Node** children;  // Recovered subtree
    char* message;
};
```

_Errors are values in the IR, not exceptions. Parser continues after errors. LSP can provide completions on broken code. Essential for good UX._

