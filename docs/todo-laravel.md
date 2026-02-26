# PHPantom — Laravel Support: Remaining Work

> Last updated: 2025-07-21

This document tracks bugs, known gaps, and missing features in
PHPantom's Laravel Eloquent support. For the general architecture and
virtual member provider design, see `ARCHITECTURE.md`.

---

## Missing features

### 1. Variable assignment from builder-forwarded static method in GTD

`$q = User::where(...)` then `$q->orderBy()` does not fully resolve for
go-to-definition because the variable resolution path
(`resolve_rhs_static_call`) finds `where()` on the raw `Task` class via
`resolve_method_return_types_with_args`, which calls
`resolve_class_fully` internally. The issue is that the returned Builder
type's methods are resolved, but go-to-definition then cannot trace back
to the declaring class in a Builder loaded through the chain. This
works for completion (which only needs the type) but not for GTD (which
needs the source location).

### 2. Closure parameter inference in collection pipelines

`$users->map(fn($u) => $u->...)` does not infer `$u` as the
collection's element type. This is a general generics/callable
inference problem, not Laravel-specific, but Laravel collection
pipelines are the most common place users encounter it.
Other cases:
- MyModel::whereIn()->chunk(self::CHUNK_SIZE, function (Collection $orders) {})
- MyModel::whereHas('order', function (Builder $q) {})
- MyModel::with(['translations' => function (Relation $query) {}]) // translations is the name of the relation on MyModel, Relation will become the return type of that relation

### 3. Go-to-definition for scope methods called through `with()`

`Brand::with('english')->sortable()` does not resolve go-to-definition
for `sortable()`, even though completion works. Compare with
`Brand::with('english')->paginate()` where GTD works fine. The
difference is that `paginate()` is a real Builder method with a source
location, while `sortable()` is a scope method injected onto the
Builder at resolution time without preserving the original declaration
site. The GTD fallback `find_scope_on_builder_model` may not be
triggering for the `with()` return path.

### 4. Multi-line chain after `with()` breaks completion and GTD

When the chain after `with()` is split across lines, neither completion
nor go-to-definition works:

```php
Brand::with('english')
    ->paginate(); // neither GTD nor completion works
```

The single-line equivalent resolves fine. This is a variant of the
multi-line closure argument issue (now fixed): `collapse_continuation_lines`
joins lines that start with `->`, but the base line `Brand::with('english')`
is not being found when the chain spans lines in this specific pattern.
Likely a cursor-position or line-counting edge case in the collapse logic.

---

## Out of scope (and why)

| Item | Reason |
|------|--------|
| Container string aliases | Requires booting the application. Use `::class` references instead. |
| Facade `getFacadeAccessor()` with string aliases | Same problem. `@method` tags provide a workable fallback. |
| Blade templates | Large scope, separate project. |
| Model column types from DB/migrations | Unreasonable complexity. Require `@property` annotations (via ide-helper or hand-written). |
| Legacy Laravel versions | We target current Larastan-style annotations. Older code may degrade gracefully. |
| Application provider scanning | Low-value, high-complexity. |

---

## Philosophy (unchanged)

- **No application booting.** We never boot a Laravel application to
  resolve types.
- **No SQL/migration parsing.** Model column types are not inferred from
  database schemas or migration files.
- **Larastan-style hints preferred.** We expect relationship methods to be
  annotated in the style that Larastan expects. Fallback heuristics
  are best-effort.
- **Facades fall back to `@method`.** Facades whose `getFacadeAccessor()`
  returns a string alias cannot be resolved. `@method` tags on facade
  classes provide completion without template intelligence.
