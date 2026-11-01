use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use cow_utils::CowUtils;
use ignore::gitignore::Gitignore;
use tower_lsp_server::gen_lsp_types::{
    DocumentFormattingProvider, MessageType, Pattern, Range, ServerCapabilities, TextEdit, Uri,
};
use tracing::{debug, warn};

use oxc_language_server::{
    Capabilities, ClientMessage, LanguageId, TextDocument, Tool, ToolBuildResult, ToolBuilder,
    ToolRestartChanges, offset_to_position, uri_utils::uri_to_file_path,
    utils::normalize_user_config_path_to_watch_pattern,
};

#[cfg(not(feature = "napi"))]
use oxc_config::{ConfigDiscovery, is_js_config_path};

use crate::core::{
    ConfigScopes, FormatResult, Language, ResolveOutcome, SourceFormatter,
    build_global_ignore_matchers, config_discovery, is_ignored, resolve_ignore_paths, utils,
};
#[cfg(feature = "napi")]
use crate::core::{ExternalServices, JsConfigLoaderCb};
use crate::lsp::create_fake_file_path;
use crate::lsp::options::FormatOptions as LSPFormatOptions;

#[cfg_attr(not(feature = "napi"), derive(Default))]
pub struct ServerFormatterBuilder {
    #[cfg(feature = "napi")]
    js_config_loader: JsConfigLoaderCb,
    #[cfg(feature = "napi")]
    external_services: ExternalServices,
}

impl ServerFormatterBuilder {
    #[cfg(feature = "napi")]
    pub fn new(js_config_loader: JsConfigLoaderCb, external_services: ExternalServices) -> Self {
        Self { js_config_loader, external_services }
    }

    /// Create a dummy `ServerFormatterBuilder` for testing.
    #[cfg(all(test, not(feature = "napi")))]
    pub fn dummy() -> Self {
        Self::default()
    }

    /// Create a dummy `ServerFormatterBuilder` for testing.
    #[cfg(all(test, feature = "napi"))]
    pub fn dummy() -> Self {
        Self {
            js_config_loader: std::sync::Arc::new(|_| {
                Err("JS config not supported in tests".to_string())
            }),
            external_services: ExternalServices::dummy(),
        }
    }

    /// Creates a new `ServerFormatter` instance based on the provided root URI and options.
    /// Returns a tuple containing the `ServerFormatter` instance and optional messages to be sent to the client.
    /// These messages will be used to inform about misconfiguration.
    ///
    /// # Panics
    /// Panics if the root URI cannot be converted to a file path.
    #[cfg_attr(not(feature = "napi"), expect(clippy::unused_self))]
    pub fn build(
        &self,
        root_uri: &Uri,
        options: serde_json::Value,
    ) -> (ServerFormatter, Vec<ClientMessage>) {
        let options = deserialize_lsp_options(options);

        let root_path = uri_to_file_path(root_uri).unwrap();
        debug!("root_path = {:?}", root_path.display());

        // Resolve workspace-level concerns only here.
        // Per-file config resolution is deferred to format time.

        // If `configPath` is explicitly set, load it eagerly as the single config for all files.
        let use_nested_config = options.use_nested_configs();
        let explicit_config_path = options.config_path.as_ref().map(PathBuf::from);

        let num_of_threads = 1; // Single threaded for LSP
        // Use `block_in_place()` to avoid nested async runtime access
        #[cfg(feature = "napi")]
        if let Err(err) =
            tokio::task::block_in_place(|| self.external_services.init(num_of_threads))
        {
            tracing::error!("Failed to setup external services.\n{err}\n");
        }
        let source_formatter = SourceFormatter::new(num_of_threads);
        #[cfg(feature = "napi")]
        let source_formatter =
            source_formatter.with_external_services(Some(self.external_services.clone()));

        let (formatter, client_message) = ServerFormatter::new(
            root_path.to_path_buf(),
            source_formatter,
            #[cfg(feature = "napi")]
            JsConfigLoaderCb::clone(&self.js_config_loader),
            explicit_config_path,
            use_nested_config,
        );

        (formatter, client_message.into_iter().collect())
    }
}

impl ToolBuilder for ServerFormatterBuilder {
    fn server_capabilities(
        &self,
        capabilities: &mut ServerCapabilities,
        _backend_capabilities: &mut Capabilities,
    ) {
        capabilities.document_formatting_provider = Some(DocumentFormattingProvider::Bool(true));
    }

    fn build(&self, root_uri: &Uri, options: serde_json::Value) -> ToolBuildResult {
        let (tool, client_messages) = self.build(root_uri, options);
        ToolBuildResult { tool: Box::new(tool), client_messages }
    }
}

// ---

pub struct ServerFormatter {
    root_path: PathBuf,
    source_formatter: SourceFormatter,
    #[cfg(feature = "napi")]
    js_config_loader: JsConfigLoaderCb,
    /// Explicit `fmt.configPath` from LSP settings.
    /// When set, all files use this single config.
    explicit_config_path: Option<PathBuf>,
    /// Whether nested config discovery is active.
    /// Disabled by an explicit `fmt.configPath` or `fmt.disableNestedConfig` in LSP settings.
    use_nested_config: bool,
    /// Current config snapshot. Swapped wholesale on watched-file changes.
    ///
    /// Held behind `RwLock<Arc<_>>` so a watch event can swap a fresh state in
    /// without blocking concurrent format requests:
    /// in-flight readers clone the `Arc` and continue using the old snapshot,
    /// while subsequent reads see the new one.
    state: RwLock<Arc<FormatterState>>,
}

/// Per-rebuild snapshot of everything read from workspace files.
struct FormatterState {
    scopes: ConfigScopes,
    /// `.prettierignore` matchers (workspace-level, shared across all scopes).
    ignore_matchers: Vec<Gitignore>,
    /// The root config is a JS/TS config that failed to load.
    /// Formatting with the default options instead would rewrite files in the wrong style.
    #[cfg(not(feature = "napi"))]
    formatting_disabled: bool,
}

impl Tool for ServerFormatter {
    /// # Panics
    /// Panics if the root URI cannot be converted to a file path.
    fn handle_configuration_change(
        &self,
        builder: &dyn ToolBuilder,
        root_uri: &Uri,
        old_options_json: &serde_json::Value,
        new_options_json: serde_json::Value,
    ) -> ToolRestartChanges {
        let old_option = deserialize_lsp_options(old_options_json.clone());
        let new_option = deserialize_lsp_options(new_options_json.clone());

        if old_option == new_option {
            return ToolRestartChanges {
                tool: None,
                watch_patterns: None,
                client_messages: Vec::new(),
            };
        }

        builder.shutdown(root_uri);
        let ToolBuildResult { tool, client_messages } =
            builder.build(root_uri, new_options_json.clone());
        let watch_patterns = tool.get_watcher_patterns(new_options_json);
        ToolRestartChanges {
            tool: Some(tool),
            watch_patterns: Some(watch_patterns),
            client_messages,
        }
    }

    fn get_watcher_patterns(&self, options: serde_json::Value) -> Vec<Pattern> {
        let options = deserialize_lsp_options(options);

        let mut patterns: Vec<Pattern> = if let Some(config_path) = options.config_path.as_deref() {
            vec![normalize_user_config_path_to_watch_pattern(config_path)]
        } else {
            let discovery = config_discovery(&self.root_path);
            // Watch subdirectories too for nested config support;
            // with `disableNestedConfig` or in Vite+ mode, only the workspace-root config is used
            let prefix =
                if options.use_nested_configs() && discovery.nested_configs() { "**/" } else { "" };
            let patterns = discovery
                .config_file_names()
                .into_iter()
                .map(|name| format!("{prefix}{name}"))
                .collect();
            #[cfg(not(feature = "napi"))]
            let patterns = with_vite_config_names(patterns);
            patterns
        };

        patterns.push(".editorconfig".to_string());
        patterns.push(".prettierignore".to_string());
        patterns
    }

    fn handle_watched_file_change(
        &self,
        _builder: &dyn ToolBuilder,
        _changed_uri: &Uri,
        _root_uri: &Uri,
        _options: serde_json::Value,
    ) -> ToolRestartChanges {
        // Rebuild the snapshot wholesale.
        //
        // `NestedConfigCtx` has no per-entry invalidation API by design (its caches live as long as the `ConfigScopes`).
        // Rebuilding is cheap: the ctx itself starts empty (lazy probes),
        // and only the root resolver, `.editorconfig` and `.prettierignore` do eager file IO.
        // The trade-off is over-eviction, a config change in one nested dir also drops cached probes elsewhere.
        // But format requests are sporadic enough that lazy re-population costs nothing observable.
        let (new_state, client_message) = Self::build_state(
            &self.root_path,
            self.explicit_config_path.as_deref(),
            self.use_nested_config,
            #[cfg(feature = "napi")]
            &self.js_config_loader,
        );
        *self.state.write().expect("state rwlock poisoned") = Arc::new(new_state);

        ToolRestartChanges {
            tool: None,
            watch_patterns: None,
            client_messages: client_message.into_iter().collect(),
        }
    }

    fn run_format(&self, document: TextDocument) -> Result<Vec<TextEdit>, String> {
        #[cfg(not(feature = "napi"))]
        if self.snapshot().formatting_disabled {
            debug!("Formatting is disabled by a JS/TS root config that failed to load");
            return Ok(vec![]);
        }

        let file_content;
        let (result, source_text) = if document.uri.scheme().as_str() == "file" {
            let Some(path) = uri_to_file_path(document.uri) else {
                return Err("Invalid file URI".to_string());
            };

            let source_text = if let Some(c) = document.text.as_deref() {
                c
            } else {
                file_content = utils::read_to_string(&path)
                    .map_err(|e| format!("Failed to read file: {e}"))?;
                &file_content
            };

            let Some(result) = self.format_file(&path, source_text)? else {
                return Ok(vec![]); // No formatting for this file (unsupported or ignored)
            };

            (result, source_text)
        } else {
            let source_text = document
                .text
                .as_deref()
                .ok_or_else(|| "In-memory formatting requires content".to_string())?;

            let Some(result) =
                self.format_in_memory(document.uri, source_text, &document.language_id)?
            else {
                return Ok(vec![]); // currently not supported
            };
            (result, source_text)
        };

        // Handle result
        match result {
            FormatResult::Success { code, is_changed } => {
                if !is_changed {
                    return Ok(vec![]);
                }

                let (start, end, replacement) = compute_minimal_text_edit(source_text, &code);
                let start_position = offset_to_position(source_text, start);
                let end_position = offset_to_position(source_text, end);

                Ok(vec![TextEdit::new(
                    Range::new(start_position, end_position),
                    replacement.to_string(),
                )])
            }
            FormatResult::Error(_) => {
                // Errors should not be returned to the user.
                // The user probably wanted to format while typing incomplete code.
                Ok(Vec::new())
            }
        }
    }
}

impl ServerFormatter {
    pub fn new(
        root_path: PathBuf,
        source_formatter: SourceFormatter,
        #[cfg(feature = "napi")] js_config_loader: JsConfigLoaderCb,
        explicit_config_path: Option<PathBuf>,
        use_nested_config: bool,
    ) -> (Self, Vec<ClientMessage>) {
        let (state, client_message) = Self::build_state(
            &root_path,
            explicit_config_path.as_deref(),
            use_nested_config,
            #[cfg(feature = "napi")]
            &js_config_loader,
        );
        (
            Self {
                root_path,
                source_formatter,
                #[cfg(feature = "napi")]
                js_config_loader,
                explicit_config_path,
                use_nested_config,
                state: RwLock::new(Arc::new(state)),
            },
            client_message,
        )
    }

    /// Build a fresh [`FormatterState`] from scratch.
    ///
    /// Called once in [`Self::new`] and again on every watched-file change.
    /// `.editorconfig` and `.prettierignore` are re-resolved here so add/remove events are picked up
    /// without a separate code path.
    ///
    /// LSP must keep editing usable even when the user's config is broken,
    /// so the root falls back to the default empty config with a warning instead of bubbling the error up.
    /// Likewise, a broken `.prettierignore` is skipped with a warning.
    /// Without `napi`, a JS/TS root config that fails to load disables formatting instead (see [`FormatterState`]).
    fn build_state(
        root_path: &Path,
        explicit_config_path: Option<&Path>,
        use_nested_config: bool,
        #[cfg(feature = "napi")] js_config_loader: &JsConfigLoaderCb,
    ) -> (FormatterState, Vec<ClientMessage>) {
        let mut client_message = vec![];
        #[cfg(not(feature = "napi"))]
        let mut formatting_disabled = false;
        let scopes = match ConfigScopes::load(
            root_path,
            explicit_config_path,
            use_nested_config,
            #[cfg(feature = "napi")]
            Some(js_config_loader),
        ) {
            Ok(scopes) => scopes,
            Err(err) => {
                #[cfg(not(feature = "napi"))]
                let err = if root_config_is_js(root_path, explicit_config_path) {
                    formatting_disabled = true;
                    format!("{err}\nFormatting is disabled for this workspace.")
                } else {
                    err
                };
                client_message.push(ClientMessage {
                    message: format!(
                        "{}: {err}",
                        root_path.to_string_lossy().cow_replace('\\', "/")
                    ),
                    r#type: MessageType::Error,
                });
                ConfigScopes::with_default_root(
                    root_path,
                    use_nested_config,
                    #[cfg(feature = "napi")]
                    Some(js_config_loader),
                )
            }
        };

        // NOTE: `.gitignore` is intentionally NOT included here.
        // An LSP document is explicitly formatted by the user.
        // (CLI also formats explicitly specified git ignored files.)
        let ignore_matchers = match resolve_ignore_paths(root_path, &[])
            .and_then(|paths| build_global_ignore_matchers(root_path, &[], &paths))
        {
            Ok(matchers) => matchers,
            Err(err) => {
                client_message.push(ClientMessage {
                    message: format!("Failed to load .prettierignore: {err}"),
                    r#type: MessageType::Error,
                });
                vec![]
            }
        };

        let state = FormatterState {
            scopes,
            ignore_matchers,
            #[cfg(not(feature = "napi"))]
            formatting_disabled,
        };
        (state, client_message)
    }

    /// Snapshot the current state.
    /// In-flight reads survive a concurrent rebuild because the old `Arc` keeps the previous snapshot alive.
    fn snapshot(&self) -> Arc<FormatterState> {
        Arc::clone(&self.state.read().expect("state rwlock poisoned"))
    }

    /// Resolve config and format a file at the given path.
    /// Returns `None` if the file is unsupported or ignored.
    ///
    /// `language` is given for an in-memory document (from its `languageId`),
    /// which then decides the formatter, see [`crate::core::ConfigResolver::classify`].
    fn resolve_and_format(
        &self,
        scopes: &ConfigScopes,
        path: &Path,
        source_text: &str,
        language: Option<Language>,
    ) -> Result<Option<FormatResult>, String> {
        let resolver = match scopes.resolve(path) {
            Ok(r) => r,
            // The root config's style would be wrong for this scope
            #[cfg(not(feature = "napi"))]
            Err(err) if nearest_config_is_js(path, &config_discovery(&self.root_path)) => {
                return Err(err);
            }
            Err(err) => {
                warn!(
                    "Failed to resolve nested config for {}: {err}, falling back to root",
                    path.display()
                );
                Arc::clone(scopes.root())
            }
        };

        if resolver.is_path_ignored(path, false) {
            debug!("File is ignored by config ignorePatterns: {}", path.display());
            return Ok(None);
        }

        let Some(strategy) = resolver.classify(path, language) else {
            debug!("Unsupported file type for formatting: {}", path.display());
            return Ok(None);
        };
        let plan = match resolver.resolve(path, strategy) {
            Ok(ResolveOutcome::Format(plan)) => plan,
            Ok(ResolveOutcome::MissingPlugin(plugin)) => {
                return Err(format!(
                    "`{plugin}` plugin is not enabled in resolved config: {}",
                    path.display()
                ));
            }
            Err(err) => {
                return Err(format!("Config resolve error for {}: {err}", path.display()));
            }
        };
        debug!("plan = {plan:?}");

        Ok(Some(tokio::task::block_in_place(|| self.source_formatter.format(source_text, plan))))
    }

    fn format_file(&self, path: &Path, source_text: &str) -> Result<Option<FormatResult>, String> {
        let state = self.snapshot();
        if is_ignored(&state.ignore_matchers, path, false, true) {
            debug!("File is ignored by .prettierignore: {}", path.display());
            return Ok(None);
        }
        self.resolve_and_format(&state.scopes, path, source_text, None)
    }

    fn format_in_memory(
        &self,
        uri: &Uri,
        source_text: &str,
        language_id: &LanguageId,
    ) -> Result<Option<FormatResult>, String> {
        let Some(language) = Language::from_lsp_id(language_id.as_str()) else {
            debug!("Unsupported language id for in-memory formatting: {language_id:?}");
            return Ok(None);
        };
        let path = create_fake_file_path(language, &self.root_path, uri);
        self.resolve_and_format(&self.snapshot().scopes, &path, source_text, Some(language))
    }
}

// ---

/// Adds the workspace-root `vite.config.*` names, which can turn Vite+ mode on (see [`config_discovery`]).
#[cfg(not(feature = "napi"))]
fn with_vite_config_names(mut patterns: Vec<Pattern>) -> Vec<Pattern> {
    for name in ConfigDiscovery::vite_plus().config_file_names() {
        if !patterns.iter().any(|pattern| pattern == name) {
            patterns.push(name.to_string());
        }
    }
    patterns
}

/// Whether the workspace root config is a JS/TS config, given or discovered like [`ConfigScopes::load`].
#[cfg(not(feature = "napi"))]
fn root_config_is_js(root_path: &Path, explicit_config_path: Option<&Path>) -> bool {
    match explicit_config_path {
        Some(path) => is_js_config_path(path),
        None => nearest_config_is_js(root_path, &config_discovery(root_path)),
    }
}

/// Whether the nearest config file upwards from `path` is a JS/TS config.
#[cfg(not(feature = "napi"))]
fn nearest_config_is_js(path: &Path, discovery: &ConfigDiscovery) -> bool {
    let nearest =
        path.ancestors().find_map(|dir| discovery.find_unique_config_by_readdir(dir).transpose());
    matches!(nearest, Some(Ok(config)) if is_js_config_path(config.path()))
}

/// Deserialize `LSPFormatOptions` from JSON, falling back to defaults on failure.
fn deserialize_lsp_options(value: serde_json::Value) -> LSPFormatOptions {
    match serde_json::from_value::<LSPFormatOptions>(value) {
        Ok(opts) => opts,
        Err(err) => {
            warn!(
                "Failed to deserialize LSPFormatOptions from JSON: {err}, falling back to default options"
            );
            LSPFormatOptions::default()
        }
    }
}

/// Returns the minimal text edit (start, end, replacement) to transform `source_text` into `formatted_text`
///
/// Respects EOL characters as a whole edit. Appending `\r` before `\n` is invalid and should be treated as a single edit replacing `\n` with `\r\n`.
#[expect(clippy::cast_possible_truncation)]
fn compute_minimal_text_edit<'a>(
    source_text: &str,
    formatted_text: &'a str,
) -> (u32, u32, &'a str) {
    debug_assert_ne!(
        source_text, formatted_text,
        "compute_minimal_text_edit: source_text and formatted_text must be different"
    );

    // Find common prefix (byte offset)
    let mut prefix_byte = 0;
    for (a, b) in source_text.chars().zip(formatted_text.chars()) {
        if a == b {
            prefix_byte += a.len_utf8();
        } else {
            break;
        }
    }

    // Find common suffix (byte offset from end)
    let mut suffix_byte = 0;
    let src_bytes = source_text.as_bytes();
    let fmt_bytes = formatted_text.as_bytes();
    let src_len = src_bytes.len();
    let fmt_len = fmt_bytes.len();

    while suffix_byte < src_len - prefix_byte && suffix_byte < fmt_len - prefix_byte {
        let src_idx = src_len - 1 - suffix_byte;
        let fmt_idx = fmt_len - 1 - suffix_byte;

        // Prevents appending `\r` to a line ending in `\n`, instead we want to replace `\n` with `\r\n` in this case,
        // aligning how LSP text documents are represented (line / column editing, instead of bytes).
        if src_bytes[src_idx] == b'\n' && fmt_bytes[fmt_idx] == b'\n' {
            let src_has_cr = src_idx > 0 && src_bytes[src_idx - 1] == b'\r';
            let fmt_has_cr = fmt_idx > 0 && fmt_bytes[fmt_idx - 1] == b'\r';

            if src_has_cr != fmt_has_cr {
                break;
            }
        }

        if src_bytes[src_idx] != fmt_bytes[fmt_idx] {
            break;
        }

        suffix_byte += 1;
    }

    let start = prefix_byte as u32;
    let end = (src_len - suffix_byte) as u32;
    let replacement_start = prefix_byte;
    let replacement_end = fmt_len - suffix_byte;
    let replacement = &formatted_text[replacement_start..replacement_end];

    (start, end, replacement)
}

// ---

#[cfg(test)]
mod tests_builder {
    use crate::lsp::server_formatter::ServerFormatterBuilder;
    use oxc_language_server::{Capabilities, ToolBuilder};
    use tower_lsp_server::gen_lsp_types::DocumentFormattingProvider;

    #[test]
    fn test_server_capabilities() {
        use tower_lsp_server::gen_lsp_types::ServerCapabilities;

        let builder = ServerFormatterBuilder::dummy();
        let mut capabilities = ServerCapabilities::default();

        builder.server_capabilities(&mut capabilities, &mut Capabilities::default());

        assert_eq!(
            capabilities.document_formatting_provider,
            Some(DocumentFormattingProvider::Bool(true))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::compute_minimal_text_edit;
    use oxc_language_server::offset_to_position;

    #[test]
    #[should_panic(
        expected = "compute_minimal_text_edit: source_text and formatted_text must be different"
    )]
    fn test_no_change() {
        let src = "abc";
        let formatted = "abc";
        compute_minimal_text_edit(src, formatted);
    }

    #[test]
    fn test_single_char_change() {
        let src = "abc";
        let formatted = "axc";
        let (start, end, replacement) = compute_minimal_text_edit(src, formatted);
        // Only 'b' replaced by 'x'
        assert_eq!((start, end, replacement), (1, 2, "x"));
    }

    #[test]
    fn test_insert_char() {
        let src = "abc";
        let formatted = "abxc";
        let (start, end, replacement) = compute_minimal_text_edit(src, formatted);
        // Insert 'x' after 'b'
        assert_eq!((start, end, replacement), (2, 2, "x"));
    }

    #[test]
    fn test_delete_char() {
        let src = "abc";
        let formatted = "ac";
        let (start, end, replacement) = compute_minimal_text_edit(src, formatted);
        // Delete 'b'
        assert_eq!((start, end, replacement), (1, 2, ""));
    }

    #[test]
    fn test_replace_multiple_chars() {
        let src = "abcdef";
        let formatted = "abXYef";
        let (start, end, replacement) = compute_minimal_text_edit(src, formatted);
        // Replace "cd" with "XY"
        assert_eq!((start, end, replacement), (2, 4, "XY"));
    }

    #[test]
    fn test_replace_multiple_chars_between_similars_complex() {
        let src = "aYabYb";
        let formatted = "aXabXb";
        let (start, end, replacement) = compute_minimal_text_edit(src, formatted);
        assert_eq!((start, end, replacement), (1, 5, "XabX"));
    }

    #[test]
    fn test_unicode() {
        let src = "a😀b";
        let formatted = "a😃b";
        let (start, end, replacement) = compute_minimal_text_edit(src, formatted);
        // Replace 😀 with 😃
        assert_eq!((start, end, replacement), (1, 5, "😃"));
    }

    #[test]
    fn test_lsp_edit_range_ignores_unicode_line_separators() {
        let src = "a\u{2028}b\u{2029}c \nnext";
        let formatted = "a\u{2028}b\u{2029}c\nnext";
        let (start, end, replacement) = compute_minimal_text_edit(src, formatted);

        let start_position = offset_to_position(src, start);
        let end_position = offset_to_position(src, end);

        assert_eq!((start_position.line, start_position.character), (0, 5));
        assert_eq!((end_position.line, end_position.character), (0, 6));
        assert_eq!(replacement, "");
    }

    #[test]
    fn test_append() {
        let src = "a".repeat(100);
        let mut formatted = src.clone();
        formatted.push('b'); // Add a character at the end

        let (start, end, replacement) = compute_minimal_text_edit(&src, &formatted);
        assert_eq!((start, end, replacement), (100, 100, "b"));
    }

    #[test]
    fn test_prepend() {
        let src = "a".repeat(100);
        let mut formatted = String::from("b");
        formatted.push_str(&src); // Add a character at the start

        let (start, end, replacement) = compute_minimal_text_edit(&src, &formatted);
        assert_eq!((start, end, replacement), (0, 0, "b"));
    }

    #[test]
    #[expect(clippy::cast_possible_truncation)]
    fn test_replacing_line_breaks() {
        let src_lf = "line1\nline2\nline3";
        let src_crlf = "line1\r\nline2\r\nline3";

        // from LF to CRLF
        {
            let (start, end, replacement) = compute_minimal_text_edit(src_lf, src_crlf);
            assert_eq!((start, end, replacement), (5, src_lf.len() as u32 - 5, "\r\nline2\r\n"));
        }

        // from CRLF to LF
        {
            let (start, end, replacement) = compute_minimal_text_edit(src_crlf, src_lf);
            assert_eq!((start, end, replacement), (5, src_crlf.len() as u32 - 5, "\nline2\n"));
        }
    }
}

#[cfg(test)]
mod tests_format {
    use std::{fs, path::Path, sync::Arc};

    use oxc_language_server::{
        ClientMessage, LanguageId, TextDocument, Tool, uri_utils::file_path_to_uri,
    };
    use serde_json::Value;
    use tower_lsp_server::gen_lsp_types::{TextEdit, Uri};
    #[cfg(not(feature = "napi"))]
    use {serde_json::json, std::str::FromStr, tower_lsp_server::gen_lsp_types::MessageType};

    use super::{ServerFormatter, ServerFormatterBuilder};

    fn build(root: &Path, options: Value) -> (ServerFormatter, Vec<ClientMessage>) {
        let root_uri = file_path_to_uri(root).unwrap();
        ServerFormatterBuilder::dummy().build(&root_uri, options)
    }

    fn format_uri(
        formatter: &ServerFormatter,
        uri: &Uri,
        language_id: &str,
        text: &str,
    ) -> Result<Vec<TextEdit>, String> {
        formatter.run_format(TextDocument::new(
            uri,
            LanguageId::new(language_id.to_string()),
            Some(Arc::from(text)),
        ))
    }

    fn format(
        formatter: &ServerFormatter,
        path: &Path,
        language_id: &str,
        text: &str,
    ) -> Result<Vec<TextEdit>, String> {
        format_uri(formatter, &file_path_to_uri(path).unwrap(), language_id, text)
    }

    fn messages(messages: &[ClientMessage]) -> Vec<&str> {
        messages.iter().map(|m| m.message.as_str()).collect()
    }

    #[test]
    fn formats_native_languages_with_json_config() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".oxfmtrc.json"), r#"{ "semi": false }"#).unwrap();
        let (formatter, client_messages) = build(dir.path(), Value::Null);
        assert!(client_messages.is_empty(), "{:?}", messages(&client_messages));

        // `semi: false` from `.oxfmtrc.json` removes only the `;`
        let edits =
            format(&formatter, &dir.path().join("a.ts"), "typescript", "const a = 1;\n").unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "");
        assert_eq!((edits[0].range.start.line, edits[0].range.start.character), (0, 11));
        assert_eq!((edits[0].range.end.line, edits[0].range.end.character), (0, 12));

        for (file, language_id, text) in [
            ("a.json", "json", r#"{"a":1}"#),
            ("a.css", "css", "a{color:red}"),
            ("a.scss", "scss", "a{b{color:red}}"),
            ("a.graphql", "graphql", "query{a}"),
            ("a.yaml", "yaml", "a:   1"),
            ("a.md", "markdown", "* item"),
            ("a.toml", "toml", "a=1"),
        ] {
            let edits = format(&formatter, &dir.path().join(file), language_id, text).unwrap();
            assert!(!edits.is_empty(), "`{file}` should be formatted natively");
        }
    }

    #[test]
    #[cfg(not(feature = "napi"))]
    fn prettier_languages_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (formatter, _) = build(dir.path(), Value::Null);

        for (file, language_id) in [("a.vue", "vue"), ("a.html", "html"), ("a.mdx", "mdx")] {
            let result = format(&formatter, &dir.path().join(file), language_id, "<div   ></div>");
            assert_eq!(result, Ok(vec![]), "`{file}`");
        }

        let untitled = Uri::from_str("untitled:Untitled-1").unwrap();
        assert_eq!(format_uri(&formatter, &untitled, "vue", "<div   ></div>"), Ok(vec![]));
    }

    #[cfg(not(feature = "napi"))]
    const THROWING_CONFIG: &str = "throw new Error(\"config exploded\");\nexport default {};\n";

    /// A stub `vite-plus` whose `resolveConfig` returns the config file's default export,
    /// which is all Vite+ mode needs from it.
    #[cfg(not(feature = "napi"))]
    fn install_stub_vite_plus(root: &Path) {
        let dir = root.join("node_modules").join("vite-plus");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{ "name": "vite-plus", "type": "module", "exports": "./index.js" }"#,
        )
        .unwrap();
        fs::write(
            dir.join("index.js"),
            "import { pathToFileURL } from \"node:url\";\n\
             export async function resolveConfig({ configFile }) {\n\
             \x20 return (await import(pathToFileURL(configFile).href)).default;\n\
             }\n",
        )
        .unwrap();
    }

    /// The single edit `semi: false` makes to `const a = "x";`
    #[cfg(not(feature = "napi"))]
    fn assert_only_semicolon_removed(edits: &[TextEdit]) {
        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].new_text, "");
        assert_eq!((edits[0].range.start.line, edits[0].range.start.character), (0, 13));
        assert_eq!((edits[0].range.end.line, edits[0].range.end.character), (0, 14));
    }

    #[test]
    #[cfg(not(feature = "napi"))]
    fn js_config_is_loaded_with_node() {
        let config = "const config: { semi: boolean } = { semi: false };\nexport default config;\n";
        for (config_file, options) in [
            ("oxfmt.config.ts", Value::Null),
            ("fmt.config.ts", json!({ "fmt.configPath": "./fmt.config.ts" })),
        ] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(config_file), config).unwrap();
            let (formatter, client_messages) = build(dir.path(), options);
            assert!(client_messages.is_empty(), "{:?}", messages(&client_messages));

            let file = dir.path().join("a.ts");
            let edits = format(&formatter, &file, "typescript", "const a = \"x\";\n").unwrap();
            assert_only_semicolon_removed(&edits);
        }
    }

    /// Formatting with the default options instead would rewrite files in the wrong style.
    #[test]
    #[cfg(not(feature = "napi"))]
    fn js_config_that_fails_to_load_disables_formatting() {
        for (config_file, options) in [
            ("oxfmt.config.ts", Value::Null),
            ("fmt.config.ts", json!({ "fmt.configPath": "./fmt.config.ts" })),
        ] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(config_file), THROWING_CONFIG).unwrap();
            let (formatter, client_messages) = build(dir.path(), options);

            assert_eq!(client_messages.len(), 1, "{:?}", messages(&client_messages));
            let message = &client_messages[0];
            assert_eq!(message.r#type, MessageType::Error);
            assert!(message.message.contains(config_file), "{}", message.message);
            assert!(message.message.contains("config exploded"), "{}", message.message);
            assert!(message.message.contains("Formatting is disabled"), "{}", message.message);

            // The default options would change both
            let file = dir.path().join("a.ts");
            assert_eq!(format(&formatter, &file, "typescript", "const a=1"), Ok(vec![]));
            let untitled = Uri::from_str("untitled:Untitled-1").unwrap();
            assert_eq!(format_uri(&formatter, &untitled, "typescript", "const a=1"), Ok(vec![]));
        }
    }

    #[test]
    #[cfg(not(feature = "napi"))]
    fn json_config_reenables_formatting_after_js_config_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let js_config = dir.path().join("oxfmt.config.ts");
        fs::write(&js_config, THROWING_CONFIG).unwrap();
        let (formatter, _) = build(dir.path(), Value::Null);
        let file = dir.path().join("a.ts");
        assert_eq!(format(&formatter, &file, "typescript", "const a=1"), Ok(vec![]));

        fs::remove_file(&js_config).unwrap();
        let json_config = dir.path().join(".oxfmtrc.json");
        fs::write(&json_config, "{}").unwrap();
        let changes = formatter.handle_watched_file_change(
            &ServerFormatterBuilder::dummy(),
            &file_path_to_uri(&json_config).unwrap(),
            &file_path_to_uri(dir.path()).unwrap(),
            Value::Null,
        );
        assert!(changes.client_messages.is_empty(), "{:?}", messages(&changes.client_messages));
        assert_eq!(format(&formatter, &file, "typescript", "const a=1").unwrap().len(), 1);
    }

    #[test]
    #[cfg(not(feature = "napi"))]
    fn nested_js_config_applies_to_its_scope() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".oxfmtrc.json"), "{}").unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("oxfmt.config.ts"), "export default { semi: false };\n").unwrap();
        let (formatter, client_messages) = build(dir.path(), Value::Null);
        assert!(client_messages.is_empty(), "{:?}", messages(&client_messages));

        let edits = format(&formatter, &sub.join("a.ts"), "typescript", "const a = \"x\";\n");
        assert_only_semicolon_removed(&edits.unwrap());
        let root_file = dir.path().join("b.ts");
        assert_eq!(format(&formatter, &root_file, "typescript", "const a = \"x\";\n"), Ok(vec![]));
    }

    #[test]
    #[cfg(not(feature = "napi"))]
    fn nested_js_config_that_fails_is_an_error_for_its_scope_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".oxfmtrc.json"), "{}").unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("oxfmt.config.ts"), THROWING_CONFIG).unwrap();
        let (formatter, client_messages) = build(dir.path(), Value::Null);
        assert!(client_messages.is_empty(), "{:?}", messages(&client_messages));

        // No fallback to the root config, which would format in the wrong style
        let err = format(&formatter, &sub.join("a.ts"), "typescript", "const a=1").unwrap_err();
        assert!(err.contains("oxfmt.config.ts"), "{err}");
        assert!(err.contains("config exploded"), "{err}");

        let edits =
            format(&formatter, &dir.path().join("b.ts"), "typescript", "const a=1").unwrap();
        assert_eq!(edits.len(), 1);
    }

    /// Matches `vp fmt`: only the `fmt` field of the root `vite.config.*` counts,
    /// `.oxfmtrc.json` and nested `vite.config.*` files are ignored.
    #[test]
    #[cfg(not(feature = "napi"))]
    fn vite_config_fmt_field_is_used_when_vite_plus_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        install_stub_vite_plus(dir.path());
        fs::write(dir.path().join("vite.config.mjs"), "export default { fmt: { semi: false } };\n")
            .unwrap();
        fs::write(dir.path().join(".oxfmtrc.json"), r#"{ "singleQuote": true }"#).unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("vite.config.mjs"), "export default { fmt: { singleQuote: true } };\n")
            .unwrap();
        let (formatter, client_messages) = build(dir.path(), Value::Null);
        assert!(client_messages.is_empty(), "{:?}", messages(&client_messages));

        for file in [dir.path().join("a.ts"), sub.join("b.ts")] {
            let edits = format(&formatter, &file, "typescript", "const a = \"x\";\n").unwrap();
            assert_only_semicolon_removed(&edits);
        }

        let patterns = formatter.get_watcher_patterns(Value::Null);
        assert!(patterns.contains(&"vite.config.ts".to_string()), "{patterns:?}");
        assert!(!patterns.iter().any(|p| p.contains("oxfmtrc")), "{patterns:?}");
    }

    /// Matches `vp fmt`: a `vite.config.*` without `fmt` means the defaults, not `.oxfmtrc.json`.
    #[test]
    #[cfg(not(feature = "napi"))]
    fn vite_config_without_fmt_field_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        install_stub_vite_plus(dir.path());
        fs::write(dir.path().join("vite.config.mjs"), "export default {};\n").unwrap();
        fs::write(dir.path().join(".oxfmtrc.json"), r#"{ "singleQuote": true }"#).unwrap();
        let (formatter, client_messages) = build(dir.path(), Value::Null);
        assert!(client_messages.is_empty(), "{:?}", messages(&client_messages));

        let file = dir.path().join("a.ts");
        assert_eq!(format(&formatter, &file, "typescript", "const a = \"x\";\n"), Ok(vec![]));
    }

    #[test]
    #[cfg(not(feature = "napi"))]
    fn vite_config_is_ignored_without_vite_plus() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("vite.config.mjs"), "export default { fmt: { semi: false } };\n")
            .unwrap();
        let (formatter, client_messages) = build(dir.path(), Value::Null);
        assert!(client_messages.is_empty(), "{:?}", messages(&client_messages));

        let file = dir.path().join("a.ts");
        assert_eq!(format(&formatter, &file, "typescript", "const a = \"x\";\n"), Ok(vec![]));

        // Still watched, so that adding `vite-plus` and touching the config switches modes
        let patterns = formatter.get_watcher_patterns(Value::Null);
        assert!(patterns.contains(&"vite.config.ts".to_string()), "{patterns:?}");
        assert!(patterns.contains(&"**/.oxfmtrc.json".to_string()), "{patterns:?}");
    }

    #[test]
    #[cfg(not(feature = "napi"))]
    fn invalid_vite_fmt_field_disables_formatting() {
        let dir = tempfile::tempdir().unwrap();
        install_stub_vite_plus(dir.path());
        fs::write(dir.path().join("vite.config.mjs"), "export default { fmt: 1 };\n").unwrap();
        let (formatter, client_messages) = build(dir.path(), Value::Null);

        assert_eq!(client_messages.len(), 1, "{:?}", messages(&client_messages));
        let message = &client_messages[0].message;
        assert!(message.contains("The `fmt` field"), "{message}");
        assert!(message.contains("Formatting is disabled"), "{message}");
        let file = dir.path().join("a.ts");
        assert_eq!(format(&formatter, &file, "typescript", "const a=1"), Ok(vec![]));
    }

    #[test]
    #[cfg(not(feature = "napi"))]
    fn vite_config_change_is_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        install_stub_vite_plus(dir.path());
        let config = dir.path().join("vite.config.mjs");
        fs::write(&config, "export default { fmt: { semi: false } };\n").unwrap();
        let (formatter, _) = build(dir.path(), Value::Null);
        let file = dir.path().join("a.ts");
        let edits = format(&formatter, &file, "typescript", "const a = \"x\";\n").unwrap();
        assert_only_semicolon_removed(&edits);

        fs::write(&config, "export default { fmt: {} };\n").unwrap();
        let changes = formatter.handle_watched_file_change(
            &ServerFormatterBuilder::dummy(),
            &file_path_to_uri(&config).unwrap(),
            &file_path_to_uri(dir.path()).unwrap(),
            Value::Null,
        );
        assert!(changes.client_messages.is_empty(), "{:?}", messages(&changes.client_messages));
        assert_eq!(format(&formatter, &file, "typescript", "const a = \"x\";\n"), Ok(vec![]));
    }
}
