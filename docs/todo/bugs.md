# PHPantom — Bug Fixes

Known bugs and incorrect behaviour. These are distinct from feature
requests — they represent cases where existing functionality produces
wrong results. Bugs should generally be fixed before new features at
the same impact tier.

Items are ordered by **impact** (descending), then **effort** (ascending)
within the same impact tier.

| Label      | Scale                                                                                                                  |
| ---------- | ---------------------------------------------------------------------------------------------------------------------- |
| **Impact** | **Critical**, **High**, **Medium-High**, **Medium**, **Low-Medium**, **Low**                                           |
| **Effort** | **Low** (≤ 1 day), **Medium** (2-5 days), **Medium-High** (1-2 weeks), **High** (2-4 weeks), **Very High** (> 1 month) |

---

## B11 — Diagnostic deduplication drops distinct diagnostics on the same range

| Impact | Effort |
| ------ | ------ |
| Medium | Low    |

`deduplicate_diagnostics` in `src/diagnostics/mod.rs` calls
`dedup_by(|a, b| a.range == b.range)` after sorting by range. This
removes **all** diagnostics that share the exact same span, regardless
of their diagnostic code, message, or severity. If two genuinely
different native diagnostics land on the same range (e.g. an
`argument_count` error and an `unknown_member` warning on the same
expression), the second one is silently dropped.

**Fix:** Change the dedup key from `a.range == b.range` to
`a.range == b.range && a.code == b.code`. This preserves distinct
diagnostic codes on the same span while still collapsing true
duplicates produced by different analysis phases.

---

## B12 — PHPStan cache pruning uses length-only comparison

| Impact | Effort |
| ------ | ------ |
| Low    | Low    |

In `publish_diagnostics_for_file` (`src/diagnostics/mod.rs`), the
PHPStan cache pruning step only updates the cache when
`pruned.len() != cached.len()`. If deduplication replaces one PHPStan
diagnostic with a different one at the same count (same number of
entries but different content), the cache is not updated. On the next
Phase 1 merge the stale entry would reappear.

In practice this is unlikely because pruning only ever removes entries
(never replaces them), but the check is technically incorrect.

**Fix:** Replace the length comparison with a content comparison, or
unconditionally write the pruned set back into the cache (the extra
write is negligible).

---

## B13 — Hover shows dummy symbols

| Impact | Effort |
| ------ | ------ |
| Medium | Low    |

When hovering over certain constructs the hover popup displays
internal dummy/placeholder symbols instead of filtering them out.
These symbols are not meaningful to the user and clutter the hover
output.

**Fix:** Filter out dummy symbols before building the hover response
so only real, user-relevant information is shown.

---

## B14 — Add `@throws` action inserts misaligned docblock

| Impact | Effort |
| ------ | ------ |
| Medium | Low    |

When the "Add `@throws`" code action creates a new docblock (no
existing docblock is present), the inserted block is not aligned with
the function/method it annotates. The opening `/**` and closing `*/`
use incorrect indentation, producing code that is visually broken and
fails style checks.

**Fix:** Detect the indentation of the target function/method line and
use it as the base indentation for every line of the generated
docblock (`/**`, ` * @throws …`, ` */`).

---

## B15 — Completion after `->|()` should not insert parentheses

| Impact | Effort |
| ------ | ------ |
| Medium | Low    |

When completing a method call where the cursor is immediately before
existing parentheses (e.g. `$obj->|()` with the cursor at `|`), the
completion item still inserts its own parentheses, producing
`$obj->method()()`. The completion engine should detect that
parentheses already follow the cursor and suppress the snippet
suffix in that case.

**Fix:** Before attaching the `()` (or `($1)`) snippet suffix to a
callable completion item, check whether the character immediately
after the completion range is `(`. If so, emit a plain text insert
without parentheses.

---

## B16 — PHPStan stale-diagnostic clearing is overly aggressive

| Impact | Effort |
| ------ | ------ |
| Medium | Medium |

`is_stale_phpstan_diagnostic()` in `src/diagnostics/mod.rs` sometimes
clears diagnostics that are still valid. For example, adding a
`@throws` tag for one exception may cause an unrelated diagnostic on
a nearby line to be treated as stale if the simple substring check
matches text that happens to appear elsewhere in the file. The
heuristic-based approach (checking whether a short name appears after
`@throws` anywhere in the file content) has false positives.

**Fix:** Audit each branch of `is_stale_phpstan_diagnostic()` and
tighten the checks:

1. Scope `content_has_throws_tag` to the docblock enclosing the
   diagnostic line rather than searching the entire file.
2. For `@phpstan-ignore` coverage, verify that the ignore comment is
   on the exact diagnostic line (or the line immediately before it),
   not just any line in the file.
3. Add regression tests for the false-positive scenarios.
