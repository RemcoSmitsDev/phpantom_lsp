/// LSP server trait implementation.
///
/// This module contains the `impl LanguageServer for Backend` block,
/// which handles all LSP protocol messages (initialize, didOpen, didChange,
/// didClose, completion, etc.).
///
/// **Diagnostic debouncing.** `did_open` publishes diagnostics immediately
/// (the user just opened the file, they want to see issues right away).
/// `did_change` debounces: each keystroke bumps a per-file version counter
/// and sleeps for 200 ms.  If another edit arrives before the timer fires,
/// the version counter won't match and the stale handler skips publishing.
/// tower-lsp runs each notification handler as an independent async task,
/// so the sleep only blocks that handler, not the server.
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use tower_lsp::LanguageServer;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::request::{GotoImplementationParams, GotoImplementationResponse};
use tower_lsp::lsp_types::*;

use crate::Backend;
use crate::classmap_scanner::{self, WorkspaceScanResult};
use crate::composer;
use crate::config::IndexingStrategy;

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Extract and store the workspace root path
        let workspace_root = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok());

        if let Some(root) = workspace_root {
            *self.workspace_root.write() = Some(root);
        }

        Ok(InitializeResult {
            offset_encoding: None,
            capabilities: ServerCapabilities {
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string(), ")".to_string()]),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                }),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![
                        "$".to_string(),
                        ">".to_string(),
                        ":".to_string(),
                        "@".to_string(),
                        "'".to_string(),
                        "\"".to_string(),
                        "[".to_string(),
                        " ".to_string(),
                        "\\".to_string(),
                    ]),
                    all_commit_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                    completion_item: None,
                }),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::new("source.organizeImports"),
                        ]),
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: None,
                        },
                        resolve_provider: None,
                    },
                )),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                })),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: self.name.clone(),
                version: Some(self.version.clone()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        // Parse composer.json for PSR-4 mappings if we have a workspace root
        let workspace_root = self.workspace_root.read().clone();

        if let Some(root) = workspace_root {
            // ── Load project configuration ──────────────────────────────
            // Read `.phpantom.toml` before anything else so that settings
            // (e.g. PHP version override, diagnostic toggles) are active
            // from the very first file load.
            match crate::config::load_config(&root) {
                Ok(cfg) => {
                    *self.config.lock() = cfg;
                }
                Err(e) => {
                    self.log(
                        MessageType::WARNING,
                        format!("Failed to load .phpantom.toml: {}", e),
                    )
                    .await;
                }
            }

            // Detect the target PHP version.  The config file override
            // takes precedence; otherwise fall back to composer.json.
            let php_version = self
                .config()
                .php
                .version
                .as_deref()
                .and_then(crate::types::PhpVersion::from_composer_constraint)
                .unwrap_or_else(|| composer::detect_php_version(&root).unwrap_or_default());
            self.set_php_version(php_version);

            let has_composer_json = root.join("composer.json").is_file();

            if has_composer_json {
                // ── Single-project path (root composer.json exists) ──────
                self.init_single_project(&root, php_version).await;
            } else {
                // ── Monorepo / non-Composer path ────────────────────────
                let subprojects = composer::discover_subproject_roots(&root);

                if !subprojects.is_empty() {
                    self.init_monorepo(&root, &subprojects, php_version).await;
                } else {
                    // No subprojects found — pure non-Composer workspace.
                    self.init_no_composer(&root, php_version).await;
                }
            }
        } else {
            self.log(MessageType::INFO, "PHPantom initialized!".to_string())
                .await;
        }

        // Spawn the background diagnostic worker. We build a shallow
        // clone of `self` that shares every `Arc`-wrapped field (maps,
        // caches, the diagnostic notify/pending slot) so the worker
        // sees all mutations the real Backend makes.  Non-Arc fields
        // (php_version, vendor_uri_prefixes, vendor_dir_paths) are
        // snapshotted — they are only written during init (above) and
        // never change afterwards.
        let worker_backend = self.clone_for_diagnostic_worker();
        tokio::spawn(async move {
            worker_backend.diagnostic_worker().await;
        });
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let uri = doc.uri.to_string();
        let text = Arc::new(doc.text);

        // Store file content
        self.open_files
            .write()
            .insert(uri.clone(), Arc::clone(&text));

        // Parse and update AST map, use map, and namespace map
        self.update_ast(&uri, &text);

        // Schedule diagnostics asynchronously so that the first-open
        // response is not blocked by lazy stub parsing (which can take
        // tens of seconds when many class references trigger cache-miss
        // parses).  This matches the did_change path.
        self.schedule_diagnostics(uri.clone());

        self.log(MessageType::INFO, format!("Opened file: {}", uri))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();

        if let Some(change) = params.content_changes.first() {
            let text = Arc::new(change.text.clone());

            // Update stored content
            self.open_files
                .write()
                .insert(uri.clone(), Arc::clone(&text));

            // Re-parse and update AST map, use map, and namespace map
            self.update_ast(&uri, &text);

            // Schedule diagnostics in a background task with debouncing.
            // This returns immediately so that completion, hover, and
            // signature help are never blocked by diagnostic computation.
            self.schedule_diagnostics(uri);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();

        self.open_files.write().remove(&uri);

        self.clear_file_maps(&uri);

        // Clear diagnostics so stale warnings don't linger after the file is closed
        self.clear_diagnostics_for_file(&uri).await;

        self.log(MessageType::INFO, format!("Closed file: {}", uri))
            .await;
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let position = params.text_document_position_params.position;

        let content = self.get_file_content(&uri);

        if let Some(content) = content {
            let result = crate::util::catch_panic_unwind_safe(
                "goto_definition",
                &uri,
                Some(position),
                || self.resolve_definition(&uri, &content, position),
            );

            if let Some(Some(location)) = result {
                return Ok(Some(GotoDefinitionResponse::Scalar(location)));
            }
        }

        Ok(None)
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let position = params.text_document_position_params.position;

        let content = self.get_file_content(&uri);

        if let Some(content) = content {
            let result = crate::util::catch_panic_unwind_safe(
                "goto_implementation",
                &uri,
                Some(position),
                || self.resolve_implementation(&uri, &content, position),
            );

            if let Some(Some(locations)) = result {
                if locations.len() == 1 {
                    return Ok(Some(GotoImplementationResponse::Scalar(
                        locations.into_iter().next().unwrap(),
                    )));
                }
                if !locations.is_empty() {
                    return Ok(Some(GotoImplementationResponse::Array(locations)));
                }
            }
        }

        Ok(None)
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let position = params.text_document_position_params.position;

        let content = self.get_file_content(&uri);

        if let Some(content) = content
            && let Some(hover) =
                crate::util::catch_panic_unwind_safe("hover", &uri, Some(position), || {
                    self.handle_hover(&uri, &content, position)
                })
        {
            return Ok(hover);
        }

        Ok(None)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.handle_completion(params).await
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;

        let content = self.get_file_content(&uri);

        if let Some(content) = content {
            let result =
                crate::util::catch_panic_unwind_safe("references", &uri, Some(position), || {
                    self.find_references(&uri, &content, position, include_declaration)
                });

            if let Some(locations) = result {
                return Ok(locations);
            }
        }

        Ok(None)
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri.to_string();

        let content = self.get_file_content(&uri);

        if let Some(content) = content {
            let actions = crate::util::catch_panic_unwind_safe("code_action", &uri, None, || {
                self.handle_code_action(&uri, &content, &params)
            });

            if let Some(actions) = actions
                && !actions.is_empty()
            {
                return Ok(Some(actions));
            }
        }

        Ok(None)
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let position = params.text_document_position_params.position;

        let content = self.get_file_content(&uri);

        if let Some(content) = content
            && let Some(sig_help) =
                crate::util::catch_panic_unwind_safe("signature_help", &uri, Some(position), || {
                    self.handle_signature_help(&uri, &content, position)
                })
        {
            return Ok(sig_help);
        }

        Ok(None)
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let position = params.text_document_position_params.position;

        let content = self.get_file_content(&uri);

        if let Some(content) = content {
            let result = crate::util::catch_panic_unwind_safe(
                "document_highlight",
                &uri,
                Some(position),
                || self.handle_document_highlight(&uri, &content, position),
            );

            if let Some(highlights) = result {
                return Ok(highlights);
            }
        }

        Ok(None)
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri.to_string();
        let position = params.position;

        let content = self.get_file_content(&uri);

        if let Some(content) = content {
            let result = crate::util::catch_panic_unwind_safe(
                "prepare_rename",
                &uri,
                Some(position),
                || self.handle_prepare_rename(&uri, &content, position),
            );

            if let Some(response) = result {
                return Ok(response);
            }
        }

        Ok(None)
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let position = params.text_document_position.position;
        let new_name = &params.new_name;

        let content = self.get_file_content(&uri);

        if let Some(content) = content {
            let new_name = new_name.to_string();
            let result =
                crate::util::catch_panic_unwind_safe("rename", &uri, Some(position), || {
                    self.handle_rename(&uri, &content, position, &new_name)
                });

            if let Some(edit) = result {
                return Ok(edit);
            }
        }

        Ok(None)
    }
}

// ─── Self-scan helpers ──────────────────────────────────────────────────────

impl Backend {
    // ── Initialization helpers ───────────────────────────────────────────

    /// Initialize a single-project workspace (root `composer.json` exists).
    ///
    /// This is the standard fast path: read PSR-4 mappings, build the
    /// classmap, scan autoload files.  Unchanged from the pre-monorepo
    /// behaviour except that vendor fields are now collections.
    async fn init_single_project(
        &self,
        root: &std::path::Path,
        php_version: crate::types::PhpVersion,
    ) {
        let (mappings, vendor_dir) = composer::parse_composer_json(root);
        let mapping_count = mappings.len();

        // Cache the vendor dir path so cross-file scans can skip it
        // without re-reading composer.json on every request.
        let vendor_path = root.join(&vendor_dir);
        self.add_vendor_dir(&vendor_path);

        *self.psr4_mappings.write() = mappings;

        // ── Build the classmap ──────────────────────────────────────
        let strategy = self.config().indexing.strategy;

        let (classmap, classmap_source) = match strategy {
            IndexingStrategy::None => {
                let cm = composer::parse_autoload_classmap(root, &vendor_dir);
                let source = if cm.is_empty() {
                    "none"
                } else {
                    "composer classmap"
                };
                (cm, source)
            }
            IndexingStrategy::SelfScan | IndexingStrategy::Full => {
                let scan = self.build_self_scan_composer(root, &vendor_dir);
                self.populate_autoload_indices(&scan);
                (scan.classmap, "self-scan")
            }
            IndexingStrategy::Composer => {
                let composer_cm = composer::parse_autoload_classmap(root, &vendor_dir);
                if !composer_cm.is_empty() {
                    let incomplete = self.is_classmap_incomplete(root, &composer_cm);
                    if incomplete {
                        let mut cm = composer_cm;
                        let scan = self.build_self_scan_composer(root, &vendor_dir);
                        self.populate_autoload_indices(&scan);
                        for (fqcn, path) in scan.classmap {
                            cm.entry(fqcn).or_insert(path);
                        }
                        (cm, "composer classmap + self-scan")
                    } else {
                        (composer_cm, "composer classmap")
                    }
                } else {
                    self.log(
                        MessageType::INFO,
                        "PHPantom: No Composer classmap found. Building class index. Run `composer dump-autoload -o` for faster startup.".to_string(),
                    ).await;
                    let scan = self.build_self_scan_composer(root, &vendor_dir);
                    self.populate_autoload_indices(&scan);
                    (scan.classmap, "self-scan")
                }
            }
        };

        let classmap_count = classmap.len();
        *self.classmap.write() = classmap;

        // ── Autoload files ──────────────────────────────────────────
        let autoload_count = self.scan_autoload_files(root, &vendor_dir);

        let func_index_count = self.autoload_function_index.read().len();
        let const_index_count = self.autoload_constant_index.read().len();

        let index_suffix = if func_index_count > 0 || const_index_count > 0 {
            format!(
                ", {} indexed function(s), {} indexed constant(s)",
                func_index_count, const_index_count
            )
        } else {
            String::new()
        };

        self.log(
            MessageType::INFO,
            format!(
                "PHPantom initialized! PHP {}, {} PSR-4 mapping(s), {} classmap entries ({}), {} autoload file(s){}",
                php_version, mapping_count, classmap_count, classmap_source, autoload_count, index_suffix
            ),
        )
        .await;
    }

    /// Initialize a monorepo workspace (no root `composer.json`, but
    /// subprojects with their own `composer.json` were discovered).
    ///
    /// Each subproject is processed through the Composer pipeline (PSR-4,
    /// classmap, autoload files, vendor packages).  After all subprojects
    /// are processed, a gitignore-aware full-scan picks up loose PHP files
    /// outside any subproject directory.
    async fn init_monorepo(
        &self,
        root: &std::path::Path,
        subprojects: &[(PathBuf, String)],
        php_version: crate::types::PhpVersion,
    ) {
        // Log the discovered subprojects.
        let sub_list: Vec<String> = subprojects
            .iter()
            .filter_map(|(p, _)| {
                p.strip_prefix(root)
                    .ok()
                    .map(|r| format!("  {}", r.display()))
            })
            .collect();
        self.log(
            MessageType::INFO,
            format!(
                "PHPantom: No root composer.json. Found {} Composer project(s):\n{}",
                subprojects.len(),
                sub_list.join("\n")
            ),
        )
        .await;

        // Collect subproject root paths for the skip set.
        let mut skip_dirs: HashSet<PathBuf> = HashSet::new();
        let mut total_mapping_count = 0usize;
        let mut total_autoload_count = 0usize;

        for (sub_root, vendor_dir) in subprojects {
            skip_dirs.insert(sub_root.clone());

            // ── PSR-4 mappings ──────────────────────────────────────
            let (mappings, _) = composer::parse_composer_json(sub_root);
            total_mapping_count += mappings.len();

            // Resolve base_path values to absolute paths so that
            // resolve_class_path works regardless of workspace_root.
            let abs_mappings: Vec<composer::Psr4Mapping> = mappings
                .into_iter()
                .map(|m| {
                    let abs_base = sub_root.join(&m.base_path).to_string_lossy().to_string();
                    composer::Psr4Mapping {
                        prefix: m.prefix,
                        base_path: composer::normalise_path(&abs_base),
                    }
                })
                .collect();
            {
                let mut psr4 = self.psr4_mappings.write();
                psr4.extend(abs_mappings);
            }

            // ── Classmap ────────────────────────────────────────────
            let cm = composer::parse_autoload_classmap(sub_root, vendor_dir);
            if !cm.is_empty() {
                let mut classmap = self.classmap.write();
                for (fqcn, path) in cm {
                    classmap.entry(fqcn).or_insert(path);
                }
            }

            // ── Vendor packages ─────────────────────────────────────
            let vendor_cm = classmap_scanner::scan_vendor_packages(sub_root, vendor_dir);
            if !vendor_cm.is_empty() {
                let mut classmap = self.classmap.write();
                for (fqcn, path) in vendor_cm {
                    classmap.entry(fqcn).or_insert(path);
                }
            }

            // ── Vendor dir tracking ─────────────────────────────────
            let vendor_path = sub_root.join(vendor_dir);
            self.add_vendor_dir(&vendor_path);

            // ── Autoload files ──────────────────────────────────────
            total_autoload_count += self.scan_autoload_files(sub_root, vendor_dir);

            // ── Self-scan subproject source dirs ────────────────────
            // When the Composer classmap is empty/missing for this
            // subproject, self-scan its PSR-4 directories.
            let sub_cm_empty = composer::parse_autoload_classmap(sub_root, vendor_dir).is_empty();
            if sub_cm_empty {
                let scan = self.build_self_scan_composer(sub_root, vendor_dir);
                self.populate_autoload_indices(&scan);
                let mut classmap = self.classmap.write();
                for (fqcn, path) in scan.classmap {
                    classmap.entry(fqcn).or_insert(path);
                }
            }
        }

        // Re-sort PSR-4 mappings by prefix length descending so
        // longest-prefix-first matching works.
        {
            let mut psr4 = self.psr4_mappings.write();
            psr4.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
        }

        // ── Full-scan loose files ───────────────────────────────────
        // Walk the workspace for PHP files outside any subproject
        // directory, using gitignore-aware walking.
        let scan = classmap_scanner::scan_workspace_fallback_full(root, &skip_dirs);
        self.populate_autoload_indices(&scan);
        {
            let mut classmap = self.classmap.write();
            for (fqcn, path) in scan.classmap {
                classmap.entry(fqcn).or_insert(path);
            }
        }

        let classmap_count = self.classmap.read().len();
        let func_index_count = self.autoload_function_index.read().len();
        let const_index_count = self.autoload_constant_index.read().len();

        let index_suffix = if func_index_count > 0 || const_index_count > 0 {
            format!(
                ", {} indexed function(s), {} indexed constant(s)",
                func_index_count, const_index_count
            )
        } else {
            String::new()
        };

        self.log(
            MessageType::INFO,
            format!(
                "PHPantom initialized! PHP {}, {} subproject(s), {} PSR-4 mapping(s), {} classmap entries, {} autoload file(s){}",
                php_version, subprojects.len(), total_mapping_count, classmap_count, total_autoload_count, index_suffix
            ),
        )
        .await;
    }

    /// Initialize a pure non-Composer workspace (no `composer.json`
    /// anywhere).  Full-scans all PHP files in the workspace.
    async fn init_no_composer(
        &self,
        root: &std::path::Path,
        php_version: crate::types::PhpVersion,
    ) {
        self.log(
            MessageType::INFO,
            "PHPantom: No composer.json found. Scanning workspace for PHP classes.".to_string(),
        )
        .await;

        let skip_dirs = HashSet::new();
        let scan = classmap_scanner::scan_workspace_fallback_full(root, &skip_dirs);
        self.populate_autoload_indices(&scan);

        let classmap_count = scan.classmap.len();
        *self.classmap.write() = scan.classmap;

        let func_index_count = self.autoload_function_index.read().len();
        let const_index_count = self.autoload_constant_index.read().len();

        let index_suffix = if func_index_count > 0 || const_index_count > 0 {
            format!(
                ", {} indexed function(s), {} indexed constant(s)",
                func_index_count, const_index_count
            )
        } else {
            String::new()
        };

        self.log(
            MessageType::INFO,
            format!(
                "PHPantom initialized! PHP {}, {} classmap entries (workspace scan){}",
                php_version, classmap_count, index_suffix
            ),
        )
        .await;
    }

    /// Register a vendor directory path and its URI prefix for
    /// vendor-file detection.
    fn add_vendor_dir(&self, vendor_path: &std::path::Path) {
        // Store the absolute path for filesystem-level skip logic.
        {
            let mut paths = self.vendor_dir_paths.lock();
            paths.push(vendor_path.to_path_buf());
        }
        // Store the URI prefix for URI-level skip logic (diagnostics,
        // find references, rename).
        let prefix = if let Ok(canonical) = vendor_path.canonicalize() {
            format!("file://{}/", canonical.display())
        } else {
            format!("file://{}/", vendor_path.display())
        };
        {
            let mut prefixes = self.vendor_uri_prefixes.lock();
            prefixes.push(prefix);
        }
    }

    /// Scan autoload files for a single project root and populate the
    /// autoload indices.  Returns the number of autoload file entries
    /// found.
    fn scan_autoload_files(&self, project_root: &std::path::Path, vendor_dir: &str) -> usize {
        let autoload_files = composer::parse_autoload_files(project_root, vendor_dir);
        let autoload_count = autoload_files.len();

        // Work queue + visited set for following require_once chains.
        let mut file_queue: Vec<PathBuf> = autoload_files;
        let mut visited: HashSet<PathBuf> = HashSet::new();

        while let Some(file_path) = file_queue.pop() {
            // Canonicalise to avoid revisiting the same file via
            // different relative paths.
            let canonical = file_path.canonicalize().unwrap_or(file_path);
            if !visited.insert(canonical.clone()) {
                continue;
            }

            if let Ok(content) = std::fs::read(&canonical) {
                let uri = format!("file://{}", canonical.display());

                // Lightweight byte-level scan: extract symbol names
                // without building a full AST.
                let scan = classmap_scanner::find_symbols(&content);

                // Populate function index.
                {
                    let mut idx = self.autoload_function_index.write();
                    for fqn in &scan.functions {
                        idx.entry(fqn.clone()).or_insert_with(|| canonical.clone());
                    }
                }

                // Populate constant index.
                {
                    let mut idx = self.autoload_constant_index.write();
                    for name in &scan.constants {
                        idx.entry(name.clone()).or_insert_with(|| canonical.clone());
                    }
                }

                // Populate class_index so find_or_load_class can
                // lazily parse these classes later.
                {
                    let mut idx = self.class_index.write();
                    for fqn in &scan.classes {
                        idx.entry(fqn.clone()).or_insert_with(|| uri.clone());
                    }
                }

                // Follow require_once statements to discover more files.
                let content_str = String::from_utf8_lossy(&content);
                let require_paths = composer::extract_require_once_paths(&content_str);
                if let Some(file_dir) = canonical.parent() {
                    for rel_path in require_paths {
                        let resolved = file_dir.join(&rel_path);
                        if resolved.is_file() {
                            file_queue.push(resolved);
                        }
                    }
                }
            }
        }

        // Store the visited autoload file paths for last-resort lazy
        // parsing of guarded functions.
        {
            let mut paths = self.autoload_file_paths.write();
            paths.extend(visited);
        }

        autoload_count
    }

    /// Build a workspace scan by self-scanning a Composer project's
    /// autoload directories (PSR-4 + classmap + vendor packages).
    ///
    /// Used when the Composer classmap is missing/incomplete or the
    /// indexing strategy is `"self"` / `"full"`.  The `project_root`
    /// is the directory containing `composer.json` (either the
    /// workspace root for single-project, or a subproject root for
    /// monorepo).
    fn build_self_scan_composer(
        &self,
        project_root: &std::path::Path,
        vendor_dir: &str,
    ) -> WorkspaceScanResult {
        // Read composer.json to extract autoload directories.
        let composer_path = project_root.join("composer.json");
        let json = match std::fs::read_to_string(&composer_path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        {
            Some(j) => j,
            None => {
                let skip_dirs = HashSet::new();
                return classmap_scanner::scan_workspace_fallback_full(project_root, &skip_dirs);
            }
        };

        let mut psr4_dirs: Vec<(String, PathBuf)> = Vec::new();
        let mut classmap_dirs: Vec<PathBuf> = Vec::new();

        // Extract from both "autoload" and "autoload-dev" sections.
        for section_key in &["autoload", "autoload-dev"] {
            if let Some(section) = json.get(section_key) {
                // PSR-4 entries
                if let Some(psr4) = section.get("psr-4").and_then(|p| p.as_object()) {
                    for (prefix, paths) in psr4 {
                        let normalised = if prefix.is_empty() {
                            String::new()
                        } else if prefix.ends_with('\\') {
                            prefix.clone()
                        } else {
                            format!("{prefix}\\")
                        };
                        for dir_str in json_value_to_strings(paths) {
                            let dir = project_root.join(&dir_str);
                            psr4_dirs.push((normalised.clone(), dir));
                        }
                    }
                }

                // Classmap entries
                if let Some(cm) = section.get("classmap").and_then(|c| c.as_array()) {
                    for entry in cm {
                        if let Some(dir_str) = entry.as_str() {
                            classmap_dirs.push(project_root.join(dir_str));
                        }
                    }
                }
            }
        }

        // Scan user source directories (classes only for PSR-4).
        let vendor_dir_paths = vec![project_root.join(vendor_dir)];
        let classmap =
            classmap_scanner::scan_psr4_directories(&psr4_dirs, &classmap_dirs, &vendor_dir_paths);

        // Scan vendor packages from installed.json.
        let vendor_cm = classmap_scanner::scan_vendor_packages(project_root, vendor_dir);

        let mut result = WorkspaceScanResult {
            classmap,
            ..Default::default()
        };

        for (fqcn, path) in vendor_cm {
            result.classmap.entry(fqcn).or_insert(path);
        }

        result
    }

    /// Store the function and constant indices from a workspace scan
    /// into the backend's shared maps.
    ///
    /// Only has an effect for non-Composer projects (the "no
    /// `composer.json`" scenario) where the full-scan populates
    /// function and constant entries.  For Composer projects the scan
    /// result's function and constant indices are empty because those
    /// symbols are discovered via the `autoload_files.php` scan loop
    /// in `initialized()` instead.
    fn populate_autoload_indices(&self, scan: &WorkspaceScanResult) {
        if !scan.function_index.is_empty() {
            let mut idx = self.autoload_function_index.write();
            for (fqn, path) in &scan.function_index {
                idx.entry(fqn.clone()).or_insert_with(|| path.clone());
            }
        }
        if !scan.constant_index.is_empty() {
            let mut idx = self.autoload_constant_index.write();
            for (name, path) in &scan.constant_index {
                idx.entry(name.clone()).or_insert_with(|| path.clone());
            }
        }
    }

    /// Check whether the Composer classmap is incomplete.
    ///
    /// Reads the PSR-4 namespace prefixes from the project's own
    /// `composer.json` and checks whether the classmap contains at
    /// least one entry for each prefix.  If a prefix is entirely
    /// absent, the classmap is considered incomplete (likely from a
    /// non-optimized `composer dump-autoload` that only covers vendor
    /// code).
    fn is_classmap_incomplete(
        &self,
        workspace_root: &std::path::Path,
        classmap: &HashMap<String, PathBuf>,
    ) -> bool {
        let composer_path = workspace_root.join("composer.json");
        let json = match std::fs::read_to_string(&composer_path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        {
            Some(j) => j,
            None => return false,
        };

        // Collect PSR-4 namespace prefixes from the project's own autoload.
        let mut user_prefixes: Vec<String> = Vec::new();
        for section_key in &["autoload", "autoload-dev"] {
            if let Some(psr4) = json
                .get(section_key)
                .and_then(|s| s.get("psr-4"))
                .and_then(|p| p.as_object())
            {
                for prefix in psr4.keys() {
                    if !prefix.is_empty() {
                        let normalised = if prefix.ends_with('\\') {
                            prefix.clone()
                        } else {
                            format!("{prefix}\\")
                        };
                        user_prefixes.push(normalised);
                    }
                }
            }
        }

        if user_prefixes.is_empty() {
            // No PSR-4 prefixes to check — can't determine completeness.
            return false;
        }

        // Check that at least one classmap entry exists for each prefix.
        for prefix in &user_prefixes {
            let has_entry = classmap
                .keys()
                .any(|fqcn| fqcn.starts_with(prefix.as_str()));
            if !has_entry {
                // Check that the PSR-4 source directory actually contains
                // PHP files.  If it does, the classmap is incomplete.
                if let Some(psr4_obj) = json
                    .get("autoload")
                    .and_then(|s| s.get("psr-4"))
                    .and_then(|p| p.as_object())
                    .into_iter()
                    .chain(
                        json.get("autoload-dev")
                            .and_then(|s| s.get("psr-4"))
                            .and_then(|p| p.as_object()),
                    )
                    .next()
                {
                    let raw_prefix = prefix.trim_end_matches('\\');
                    if let Some(paths) = psr4_obj
                        .get(raw_prefix)
                        .or_else(|| psr4_obj.get(prefix.as_str()))
                    {
                        for dir_str in json_value_to_strings(paths) {
                            let dir = workspace_root.join(&dir_str);
                            if dir.is_dir() {
                                return true;
                            }
                        }
                    }
                }
            }
        }

        false
    }
}

fn json_value_to_strings(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}
