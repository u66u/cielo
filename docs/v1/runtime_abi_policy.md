# Runtime ABI Policy (v1)

This document defines ABI versioning for handler evidence structs used by the C runtime:

- `CieloEvidence`
- `CieloContinuation`

Source of truth: `src/backend/cielo_runtime.h`.

## Version fields

Runtime ABI uses semantic triplet fields:

- `CIELO_RUNTIME_ABI_VERSION_MAJOR`
- `CIELO_RUNTIME_ABI_VERSION_MINOR`
- `CIELO_RUNTIME_ABI_VERSION_PATCH`

Packed value is `CIELO_RUNTIME_ABI_VERSION`.

## Compatibility rules

1. Patch bump (`x.y.z -> x.y.z+1`)
- No struct layout change.
- No calling convention change.
- Behavior-only fixes that preserve existing binaries.

2. Minor bump (`x.y.z -> x.y+1.0`)
- Backward-compatible additive changes only.
- New fields must be appended and guarded by reserved slots or explicit null/default handling.
- Existing field offsets and required semantics remain stable.

3. Major bump (`x.y.z -> x+1.0.0`)
- Any breaking change to struct layout, required fields, handler dispatch semantics, or perform/resume ABI.
- Required when old generated code cannot safely run against new runtime.

## Required bump triggers

Bump major if any of these happen:

- Reorder/remove/retag fields in `CieloEvidence` or `CieloContinuation`.
- Change interpretation of `capability_id`, `clause_count`, or `resume_once`.
- Change function signatures that generated C code depends on.

Bump minor if:

- Adding optional behavior or optional fields that old generated code can ignore safely.

## Implementation note

Generated C and runtime must agree on `cielo_runtime_abi_version()` and struct expectations. If they diverge, it is a compiler/runtime integration bug.
