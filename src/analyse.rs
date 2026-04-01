//! CLI analysis mode.
//!
//! Scans PHP files in a project and reports PHPantom's own diagnostics
//! (no PHPStan, no external tools) in a PHPStan-like table format.
//!
//! # Philosophy
//!
//! The goal is **100% type coverage**: every class, member, and function
//! call in the project should be resolvable by the LSP.  When that holds,
//! completion works everywhere with no dead spots, and downstream tools
//! like PHPStan get the type information they need to find real bugs at
//! every level.  PHPStan only complains about missing types at levels 6,
//! 9, and 10; PHPantom fills those gaps cheaply and immediately so
//! PHPStan can focus on logic errors rather than fighting incomplete
//! type information.
//!
//! The diagnostics reported here are not trying to be a static analyser.
//! They assert structural correctness: does this class exist, does this
//! member exist, does the argument count match, did you implement every
//! required method.  Bug hunting is left to dedicated tools like PHPStan
//! and Psalm.  The `analyze` command surfaces the places where the LSP
//! cannot resolve a symbol so the user can fix them and achieve (or
//! maintain) full completion coverage across the project.
//!
//! It reuses the same `Backend` initialization pipeline as the LSP
//! server, so the results match exactly what a user would see in their
//! editor.
//!
//! Only single Composer projects (root `composer.json`) are supported
//! for now.
//!
//! # Usage
//!
//! ```sh
//! phpantom_lsp analyze                     # scan entire project
//! phpantom_lsp analyze src/                # scan a subdirectory
//! phpantom_lsp analyze src/Foo.php         # scan a single file
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use tower_lsp::lsp_types::*;

use crate::parser::with_parse_cache;
use crate::virtual_members::with_active_resolved_class_cache;

use crate::Backend;
use crate::composer;
use crate::config;

/// Severity filter for the analyse output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeverityFilter {
    /// Show all diagnostics (error, warning, information, hint).
    All,
    /// Show only errors and warnings.
    Warning,
    /// Show only errors.
    Error,
}

/// Options for the analyse command.
#[derive(Debug)]
pub struct AnalyseOptions {
    /// Workspace root (project directory containing composer.json).
    pub workspace_root: PathBuf,
    /// Optional path filter: only analyse files under this path.
    /// Can be a directory or a single file.
    pub path_filter: Option<PathBuf>,
    /// Minimum severity to report.
    pub severity_filter: SeverityFilter,
    /// Whether to output with ANSI colours.
    pub use_colour: bool,
}

/// A single diagnostic result for the analyse output.
struct FileDiagnostic {
    /// 1-based line number.
    line: u32,
    /// The diagnostic message.
    message: String,
    /// The diagnostic code (e.g. "unknown_class").
    identifier: Option<String>,
}

/// Run the analyse command and return the process exit code.
///
/// Returns `0` when no diagnostics are found, `1` when diagnostics exist.
pub async fn run(options: AnalyseOptions) -> i32 {
    let root = &options.workspace_root;

    if !root.join("composer.json").is_file() {
        eprintln!("Error: no composer.json found in {}", root.display());
        eprintln!("The analyse command currently only supports single Composer projects.");
        return 1;
    }

    // ── 1. Load config ──────────────────────────────────────────────
    let cfg = match config::load_config(root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Warning: failed to load .phpantom.toml: {e}");
            config::Config::default()
        }
    };

    // ── 2. Index project ────────────────────────────────────────────
    // Create a headless Backend (no LSP client) and run the same init
    // pipeline as the LSP server.  With client=None the log/progress
    // calls are no-ops.
    let backend = Backend::new_headless();
    *backend.workspace_root().write() = Some(root.to_path_buf());
    *backend.config.lock() = cfg.clone();

    let composer_package = composer::read_composer_package(root);

    let php_version = cfg
        .php
        .version
        .as_deref()
        .and_then(crate::types::PhpVersion::from_composer_constraint)
        .unwrap_or_else(|| {
            composer_package
                .as_ref()
                .and_then(composer::detect_php_version_from_package)
                .unwrap_or_default()
        });
    backend.set_php_version(php_version);

    backend
        .init_single_project(root, php_version, composer_package, None)
        .await;

    // ── 3. Locate user files (via PSR-4) and crop to path ───────────
    let files = discover_user_files(&backend, root, options.path_filter.as_deref());

    if files.is_empty() {
        eprintln!("No PHP files found.");
        return 0;
    }

    // ── 4. Two-phase parallel analysis ──────────────────────────────
    //
    // Phase 1 — **Parse**: run `update_ast` on every user file so that
    // `fqn_index`, `ast_map`, `symbol_maps`, `use_map`, `namespace_map`
    // and `class_index` are fully populated for the entire project.
    //
    // Phase 2 — **Diagnose**: collect diagnostics for every file.
    // Because all user classes are already in `fqn_index`, cross-file
    // references resolve via an O(1) hash lookup instead of falling
    // through to classmap / PSR-4 lazy loading (which takes write
    // locks and serialises threads).
    //
    // Splitting the work this way also means the diagnostic phase
    // never triggers `parse_and_cache_file` for other *user* files,
    // eliminating the main source of write-lock contention that
    // previously caused the "stuck at 99 %" stall.

    let file_count = files.len();
    let severity_filter = options.severity_filter;
    let use_colour = options.use_colour;
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    // ── Phase 1: Parse all files (parallel) ─────────────────────────
    // Read each file from disk and call `update_ast`.  Store the
    // (uri, content) pairs so Phase 2 can reuse them without re-reading.
    //
    // Parsing is fast, so the progress bar is drawn at 0% before Phase 1
    // and only advances during Phase 2 (the expensive diagnostic pass).
    if use_colour {
        eprint!("\r\x1b[2K {}", progress_bar(0, file_count));
    }
    let next_idx = AtomicUsize::new(0);

    let file_data: Vec<Option<(String, String)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..n_threads)
            .map(|_| {
                let backend = &backend;
                let next_idx = &next_idx;
                let files = &files;
                s.spawn(move || {
                    let mut entries: Vec<(usize, String, String)> = Vec::new();
                    loop {
                        let i = next_idx.fetch_add(1, Ordering::Relaxed);
                        if i >= file_count {
                            break;
                        }

                        let file_path = &files[i];
                        let content = match std::fs::read_to_string(file_path) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };

                        let uri = crate::util::path_to_uri(file_path);
                        backend.update_ast(&uri, &content);
                        entries.push((i, uri, content));
                    }
                    entries
                })
            })
            .collect();

        // Collect into an indexed vec so Phase 2 can iterate in the
        // same order as `files`.
        let mut indexed: Vec<Option<(String, String)>> = (0..file_count).map(|_| None).collect();
        for handle in handles {
            for (i, uri, content) in handle.join().unwrap_or_default() {
                indexed[i] = Some((uri, content));
            }
        }
        indexed
    });

    // ── Phase 2: Collect diagnostics (parallel) ─────────────────────
    // Call individual collectors directly (instead of the grouped
    // collect_slow_diagnostics) so we can time each one independently.
    let next_idx = AtomicUsize::new(0);

    let mut all_file_diagnostics: Vec<(String, Vec<FileDiagnostic>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..n_threads)
            .map(|_| {
                let backend = &backend;
                let next_idx = &next_idx;
                let files = &files;
                let file_data = &file_data;
                s.spawn(move || {
                    let mut results: Vec<(String, Vec<FileDiagnostic>)> = Vec::new();
                    loop {
                        let i = next_idx.fetch_add(1, Ordering::Relaxed);
                        if i >= file_count {
                            break;
                        }
                        if use_colour && i.is_multiple_of(20) {
                            eprint!("\r\x1b[2K {}", progress_bar(i + 1, file_count));
                        }

                        let (uri, content) = match &file_data[i] {
                            Some(pair) => (&pair.0, &pair.1),
                            None => continue, // file that failed to read
                        };

                        // Activate ONE parse cache for the entire file so
                        // all collectors share the same parsed AST.  Each
                        // collector's own `with_parse_cache` call becomes
                        // a no-op (nested guard).
                        let _parse_guard = with_parse_cache(content);
                        let _cache_guard =
                            with_active_resolved_class_cache(&backend.resolved_class_cache);
                        let _subj_guard =
                            crate::completion::resolver::with_diagnostic_subject_cache();

                        // Provide scope boundaries so the diagnostic subject
                        // cache can distinguish variables in different methods
                        // of the same class (prevents cross-method cache
                        // pollution).
                        if let Some(sm) = backend.symbol_maps.read().get(uri.as_str()) {
                            crate::completion::resolver::set_diagnostic_subject_cache_scopes(
                                sm.scopes.clone(),
                                sm.var_defs.clone(),
                                sm.narrowing_blocks.clone(),
                                sm.assert_narrowing_offsets.clone(),
                            );
                        }

                        let mut raw = Vec::new();

                        // In debug builds, time each collector and warn
                        // about slow files.  In release builds, just call
                        // the collectors directly.
                        #[cfg(debug_assertions)]
                        {
                            macro_rules! timed_collect {
                                ($name:expr, $call:expr) => {{
                                    let t0 = std::time::Instant::now();
                                    $call;
                                    (t0.elapsed(), $name)
                                }};
                            }

                            let file_start = std::time::Instant::now();
                            let timings = [
                                timed_collect!(
                                    "fast",
                                    backend.collect_fast_diagnostics(uri, content, &mut raw)
                                ),
                                timed_collect!(
                                    "unknown_class",
                                    backend
                                        .collect_unknown_class_diagnostics(uri, content, &mut raw)
                                ),
                                timed_collect!(
                                    "unknown_member",
                                    backend
                                        .collect_unknown_member_diagnostics(uri, content, &mut raw)
                                ),
                                timed_collect!(
                                    "unknown_function",
                                    backend.collect_unknown_function_diagnostics(
                                        uri, content, &mut raw,
                                    )
                                ),
                                timed_collect!(
                                    "argument_count",
                                    backend
                                        .collect_argument_count_diagnostics(uri, content, &mut raw)
                                ),
                                timed_collect!(
                                    "implementation",
                                    backend.collect_implementation_error_diagnostics(
                                        uri, content, &mut raw,
                                    )
                                ),
                                timed_collect!(
                                    "deprecated",
                                    backend.collect_deprecated_diagnostics(uri, content, &mut raw)
                                ),
                            ];

                            let file_elapsed = file_start.elapsed();
                            if file_elapsed.as_secs() >= 5 {
                                let display =
                                    files[i].strip_prefix(root).unwrap_or(&files[i]).display();
                                let breakdown: Vec<String> = timings
                                    .iter()
                                    .filter(|(d, _)| d.as_millis() > 0)
                                    .map(|(d, name)| format!("{}={:.1}s", name, d.as_secs_f64()))
                                    .collect();
                                eprintln!(
                                    "\n  \u{26a0} slow file ({:.1}s): {}\n    {}",
                                    file_elapsed.as_secs_f64(),
                                    display,
                                    breakdown.join(", "),
                                );
                            }
                        }

                        #[cfg(not(debug_assertions))]
                        {
                            backend.collect_fast_diagnostics(uri, content, &mut raw);
                            backend.collect_unknown_class_diagnostics(uri, content, &mut raw);
                            backend.collect_unknown_member_diagnostics(uri, content, &mut raw);
                            backend.collect_unknown_function_diagnostics(uri, content, &mut raw);
                            backend.collect_argument_count_diagnostics(uri, content, &mut raw);
                            backend
                                .collect_implementation_error_diagnostics(uri, content, &mut raw);
                            backend.collect_deprecated_diagnostics(uri, content, &mut raw);
                        }

                        let mut filtered: Vec<FileDiagnostic> = raw
                            .into_iter()
                            .filter_map(|d| {
                                let sev = d.severity.unwrap_or(DiagnosticSeverity::WARNING);
                                if !passes_severity_filter(sev, severity_filter) {
                                    return None;
                                }
                                let identifier = match &d.code {
                                    Some(NumberOrString::String(s)) => Some(s.clone()),
                                    _ => None,
                                };
                                Some(FileDiagnostic {
                                    line: d.range.start.line + 1,
                                    message: d.message,
                                    identifier,
                                })
                            })
                            .collect();

                        if !filtered.is_empty() {
                            filtered.sort_by_key(|d| d.line);
                            let display_path = files[i]
                                .strip_prefix(root)
                                .unwrap_or(&files[i])
                                .to_string_lossy()
                                .to_string();
                            results.push((display_path, filtered));
                        }
                    }
                    results
                })
            })
            .collect();

        let mut merged: Vec<(String, Vec<FileDiagnostic>)> = Vec::new();
        for handle in handles {
            merged.extend(handle.join().unwrap_or_default());
        }
        merged
    });

    if use_colour {
        eprint!("\r\x1b[2K {}\n", progress_bar(file_count, file_count));
    }

    // Sort by path so output order is deterministic.
    all_file_diagnostics.sort_by(|a, b| a.0.cmp(&b.0));

    let total_errors: usize = all_file_diagnostics
        .iter()
        .map(|(_, diags)| diags.len())
        .sum();

    // ── 5. Render output ────────────────────────────────────────────
    if all_file_diagnostics.is_empty() {
        print_success_box(file_count, options.use_colour);
        return 0;
    }

    for (path, diagnostics) in &all_file_diagnostics {
        print_file_table(path, diagnostics, options.use_colour);
    }

    print_error_box(total_errors, file_count, options.use_colour);

    1
}

// ── File discovery ──────────────────────────────────────────────────────────

/// Discover user PHP files to analyse.
///
/// Walks each PSR-4 source directory from `composer.json` (these only
/// cover the project's own code, not vendor).  When `path_filter` is
/// provided the results are cropped to that file or directory.
pub(crate) fn discover_user_files(
    backend: &Backend,
    workspace_root: &Path,
    path_filter: Option<&Path>,
) -> Vec<PathBuf> {
    use ignore::WalkBuilder;

    // Resolve the path filter to an absolute path.
    let abs_filter = path_filter.map(|f| {
        if f.is_relative() {
            workspace_root.join(f)
        } else {
            f.to_path_buf()
        }
    });

    // Single-file short circuit.
    if let Some(ref resolved) = abs_filter
        && resolved.is_file()
    {
        return if resolved.extension().is_some_and(|ext| ext == "php") {
            vec![resolved.clone()]
        } else {
            Vec::new()
        };
    }

    // Collect the PSR-4 source directories as absolute paths.
    let psr4 = backend.psr4_mappings().read().clone();
    let mut source_dirs: Vec<PathBuf> = psr4
        .iter()
        .map(|m| {
            let p = Path::new(&m.base_path);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                workspace_root.join(p)
            }
        })
        .filter(|p| p.is_dir())
        .collect();

    source_dirs.sort();
    source_dirs.dedup();

    let vendor_dirs: Vec<PathBuf> = backend.vendor_dir_paths.lock().clone();

    // When an explicit path filter points outside all PSR-4 source
    // directories (e.g. into vendor/), walk the filter path directly
    // instead of skipping it.  This matches PHPStan behaviour: the
    // default scan covers only user code, but an explicit override
    // scans whatever you point it at.
    let filter_overlaps_psr4 = abs_filter.as_ref().is_none_or(|fp| {
        source_dirs
            .iter()
            .any(|d| d.starts_with(fp) || fp.starts_with(d))
    });

    let dirs_to_walk: Vec<&Path> = if filter_overlaps_psr4 {
        source_dirs.iter().map(|p| p.as_path()).collect()
    } else {
        // The filter path doesn't overlap any PSR-4 dir — walk it
        // directly (no vendor exclusion since the user explicitly
        // asked for this path).
        vec![abs_filter.as_deref().unwrap()]
    };

    let mut files: Vec<PathBuf> = Vec::new();

    for dir in &dirs_to_walk {
        // If a directory filter is active and doesn't overlap with
        // this source dir, skip entirely.
        if let Some(ref fp) = abs_filter
            && fp.is_dir()
            && !dir.starts_with(fp)
            && !fp.starts_with(dir)
        {
            continue;
        }

        let skip_vendor = if filter_overlaps_psr4 {
            vendor_dirs.clone()
        } else {
            // User explicitly targeted this path — don't skip vendor
            // subdirectories within it.
            Vec::new()
        };
        let walker = WalkBuilder::new(dir)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .hidden(true)
            .parents(true)
            .ignore(true)
            .filter_entry(move |entry| {
                if entry.file_type().is_some_and(|ft| ft.is_dir())
                    && !skip_vendor.is_empty()
                    && let Ok(canonical) = entry.path().canonicalize()
                    && skip_vendor.iter().any(|v| canonical.starts_with(v))
                {
                    return false;
                }
                true
            })
            .build();

        for entry in walker.flatten() {
            let path = entry.into_path();
            if !path.is_file() || path.extension().is_none_or(|ext| ext != "php") {
                continue;
            }

            // Crop to the filter directory.
            if let Some(ref fp) = abs_filter
                && fp.is_dir()
                && !path.starts_with(fp)
            {
                continue;
            }

            files.push(path);
        }
    }

    files.sort();
    files.dedup();
    files
}

// ── Severity helpers ────────────────────────────────────────────────────────

fn passes_severity_filter(severity: DiagnosticSeverity, filter: SeverityFilter) -> bool {
    match filter {
        SeverityFilter::All => true,
        SeverityFilter::Warning => {
            matches!(
                severity,
                DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING
            )
        }
        SeverityFilter::Error => severity == DiagnosticSeverity::ERROR,
    }
}

// ── PHPStan-style table output ──────────────────────────────────────────────
//
// Mirrors Symfony Console's `Table` style used by PHPStan's
// `TableErrorFormatter` (see phpstan-src tests for exact spacing):
//
//  ------ -------------------------------------------
//   Line   src/Foo.php
//  ------ -------------------------------------------
//   15     Call to undefined method Bar::baz().
//          🪪  unknown_member
//   42     Access to property $qux on unknown class.
//          🪪  unknown_class
//  ------ -------------------------------------------

/// Print a file's diagnostics in the PHPStan table format.
fn print_file_table(path: &str, diagnostics: &[FileDiagnostic], use_colour: bool) {
    struct Row {
        line_str: String,
        lines: Vec<String>,
    }

    let mut rows: Vec<Row> = Vec::new();
    for diag in diagnostics {
        let mut message_lines = vec![diag.message.clone()];
        if let Some(ref id) = diag.identifier {
            message_lines.push(format!("\u{1faaa}  {id}"));
        }
        rows.push(Row {
            line_str: diag.line.to_string(),
            lines: message_lines,
        });
    }

    // Column widths.
    let line_col_w = rows
        .iter()
        .map(|r| r.line_str.len())
        .max()
        .unwrap_or(0)
        .max(4); // at least as wide as "Line"

    let msg_col_w = rows
        .iter()
        .flat_map(|r| r.lines.iter().map(|l| l.len()))
        .max()
        .unwrap_or(0)
        .max(path.len());

    let sep = format!(
        " {} {}",
        "-".repeat(line_col_w + 2),
        "-".repeat(msg_col_w + 2),
    );

    // Header.
    println!("{sep}");
    if use_colour {
        println!(
            "  \x1b[32m{:>line_col_w$}\x1b[0m   \x1b[32m{path}\x1b[0m",
            "Line"
        );
    } else {
        println!("  {:>line_col_w$}   {path}", "Line");
    }
    println!("{sep}");

    // Data rows.
    for row in &rows {
        for (i, msg_line) in row.lines.iter().enumerate() {
            if i == 0 {
                println!("  {:>line_col_w$}   {msg_line}", row.line_str);
            } else if use_colour {
                println!("  {:>line_col_w$}   \x1b[2m{msg_line}\x1b[0m", "");
            } else {
                println!("  {:>line_col_w$}   {msg_line}", "");
            }
        }
    }

    // Footer + blank line between files.
    println!("{sep}");
    println!();
}

/// Print the `[OK]` success box.
fn print_success_box(_file_count: usize, use_colour: bool) {
    let text = " [OK] No errors ";
    if use_colour {
        let pad = " ".repeat(text.len());
        println!();
        println!(" \x1b[30;42m{pad}\x1b[0m");
        println!(" \x1b[30;42m{text}\x1b[0m");
        println!(" \x1b[30;42m{pad}\x1b[0m");
        println!();
    } else {
        println!("{text}");
    }
}

/// Print the `[ERROR]` summary box.
fn print_error_box(total_errors: usize, _file_count: usize, use_colour: bool) {
    let label = if total_errors == 1 { "error" } else { "errors" };
    let text = format!(" [ERROR] Found {total_errors} {label} ");
    if use_colour {
        let pad = " ".repeat(text.len());
        println!();
        println!(" \x1b[97;41m{pad}\x1b[0m");
        println!(" \x1b[97;41m{text}\x1b[0m");
        println!(" \x1b[97;41m{pad}\x1b[0m");
        println!();
    } else {
        println!("{text}");
    }
}

// ── Progress bar ────────────────────────────────────────────────────────────

const BAR_WIDTH: usize = 28;

/// Render a PHPStan-style progress bar string:
/// ` 120/883 [▓▓▓▓░░░░░░░░░░░░░░░░░░░░░░░░]  13%`
fn progress_bar(done: usize, total: usize) -> String {
    let pct = if total == 0 {
        100
    } else {
        (done * 100) / total
    };
    let filled = if total == 0 {
        BAR_WIDTH
    } else {
        (done * BAR_WIDTH) / total
    };
    let empty = BAR_WIDTH - filled;

    format!(
        " {done:>width$}/{total} [{bar_fill}{bar_empty}] {pct:>3}%",
        width = total.to_string().len(),
        bar_fill = "▓".repeat(filled),
        bar_empty = "░".repeat(empty),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_filter_all_passes_everything() {
        assert!(passes_severity_filter(
            DiagnosticSeverity::ERROR,
            SeverityFilter::All
        ));
        assert!(passes_severity_filter(
            DiagnosticSeverity::WARNING,
            SeverityFilter::All
        ));
        assert!(passes_severity_filter(
            DiagnosticSeverity::INFORMATION,
            SeverityFilter::All
        ));
        assert!(passes_severity_filter(
            DiagnosticSeverity::HINT,
            SeverityFilter::All
        ));
    }

    #[test]
    fn severity_filter_warning_blocks_info_and_hint() {
        assert!(passes_severity_filter(
            DiagnosticSeverity::ERROR,
            SeverityFilter::Warning
        ));
        assert!(passes_severity_filter(
            DiagnosticSeverity::WARNING,
            SeverityFilter::Warning
        ));
        assert!(!passes_severity_filter(
            DiagnosticSeverity::INFORMATION,
            SeverityFilter::Warning
        ));
        assert!(!passes_severity_filter(
            DiagnosticSeverity::HINT,
            SeverityFilter::Warning
        ));
    }

    #[test]
    fn severity_filter_error_only() {
        assert!(passes_severity_filter(
            DiagnosticSeverity::ERROR,
            SeverityFilter::Error
        ));
        assert!(!passes_severity_filter(
            DiagnosticSeverity::WARNING,
            SeverityFilter::Error
        ));
        assert!(!passes_severity_filter(
            DiagnosticSeverity::INFORMATION,
            SeverityFilter::Error
        ));
        assert!(!passes_severity_filter(
            DiagnosticSeverity::HINT,
            SeverityFilter::Error
        ));
    }
}
