mod auth;
mod codegen;
mod curl;
mod environments;
mod executor;
mod formatter;
mod handler;
mod history;
mod parser;
mod security;
mod variables;

use std::sync::Arc;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use handler::{SharedState, State};

const SWITCH_ENV_COMMAND: &str = "rest-client.switchEnvironment";

struct RestClientLsp {
    client: Client,
    state: SharedState,
}

#[tower_lsp::async_trait]
impl LanguageServer for RestClientLsp {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                        ..Default::default()
                    },
                )),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["{".to_string()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        handler::SEND_REQUEST_COMMAND.to_string(),
                        SWITCH_ENV_COMMAND.to_string(),
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "rest-client-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "rest-client-lsp initialized")
            .await;

        self.refresh_settings().await;

        // Clear session cache on LSP start (new session = fresh state)
        clear_session_cache();
    }

    async fn shutdown(&self) -> Result<()> {
        // Clear session cache on LSP shutdown
        clear_session_cache();
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let text = params.text_document.text.clone();

        {
            let mut state = self.state.write().await;
            state.documents.insert(uri.clone(), text.clone());
            self.sync_file_variables(&mut state, &text);
        }

        self.publish_diagnostics(&uri, &text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        if let Some(change) = params.content_changes.into_iter().last() {
            let text = change.text;
            {
                let mut state = self.state.write().await;
                state.documents.insert(uri.clone(), text.clone());
                self.sync_file_variables(&mut state, &text);
            }
            self.publish_diagnostics(&uri, &text).await;
        }
    }

    async fn did_change_configuration(&self, _: DidChangeConfigurationParams) {
        self.refresh_settings().await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let mut state = self.state.write().await;
        state.documents.remove(&params.text_document.uri);
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let uri = &params.text_document.uri;
        let state = self.state.read().await;
        let text = match state.documents.get(uri) {
            Some(t) => t,
            None => return Ok(None),
        };

        let lenses = handler::code_lenses(uri, text);
        Ok(Some(lenses))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let state = self.state.read().await;
        let text = match state.documents.get(uri) {
            Some(t) => t,
            None => return Ok(None),
        };

        let location = handler::goto_definition(uri, text, position);
        Ok(location.map(GotoDefinitionResponse::Scalar))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let state = self.state.read().await;
        let text = match state.documents.get(uri) {
            Some(t) => t,
            None => return Ok(None),
        };

        Ok(handler::hover_at(text, position, &state.variable_ctx))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let state = self.state.read().await;
        let text = match state.documents.get(uri) {
            Some(t) => t,
            None => return Ok(None),
        };

        let items = handler::completions_at(text, position, &state.variable_ctx);
        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<Value>> {
        match params.command.as_str() {
            cmd if cmd == handler::SEND_REQUEST_COMMAND => {
                if let (Some(uri_val), Some(line_val)) =
                    (params.arguments.first(), params.arguments.get(1))
                {
                    let uri_str = uri_val.as_str().unwrap_or_default();
                    let line = line_val.as_u64().unwrap_or(0) as usize;

                    if let Ok(uri) = Url::parse(uri_str) {
                        match handler::execute_request(&uri, line, &self.state).await {
                            Ok(output) => {
                                match self
                                    .show_response(
                                        &uri,
                                        &output.file_content,
                                        output.request_content_type.as_deref(),
                                    )
                                    .await
                                {
                                    Ok(path) => {
                                        self.client
                                            .show_message(
                                                MessageType::INFO,
                                                format!("Request completed. Response saved to {path}"),
                                            )
                                            .await;
                                    }
                                    Err(e) => {
                                        self.client
                                            .show_message(
                                                MessageType::WARNING,
                                                format!(
                                                    "Request completed, but response file could not be saved: {e}"
                                                ),
                                            )
                                            .await;
                                    }
                                }
                                return Ok(Some(Value::String(output.formatted_response)));
                            }
                            Err(e) => {
                                self.client
                                    .show_message(
                                        MessageType::ERROR,
                                        format!("Request failed: {e}"),
                                    )
                                    .await;
                                return Ok(Some(Value::String(e)));
                            }
                        }
                    }
                }
            }
            cmd if cmd == SWITCH_ENV_COMMAND => {
                if let Some(env_name) = params.arguments.first().and_then(|v| v.as_str()) {
                    let mut state = self.state.write().await;
                    state.settings.active_environment = Some(env_name.to_string());
                    self.apply_environment_variables(&mut state);

                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!("Switched to environment: {env_name}"),
                        )
                        .await;
                    return Ok(Some(Value::String(env_name.to_string())));
                }
            }
            _ => {}
        }
        Ok(None)
    }
}

impl RestClientLsp {
    async fn show_response(
        &self,
        source_uri: &Url,
        response: &str,
        request_content_type: Option<&str>,
    ) -> std::result::Result<String, String> {
        let path = self.response_output_path(source_uri, request_content_type)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create response dir: {e}"))?;
        }
        std::fs::write(&path, response).map_err(|e| format!("write response: {e}"))?;

        let uri = self.response_open_uri(source_uri, &path)?;
        let first = self.try_show_document(&uri, false).await;
        if !first.0 {
            let second = self.try_show_document(&uri, true).await;
            if !second.0 {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!(
                            "Response saved, but editor did not open file (uri: {uri}, try1: {}, try2: {}).",
                            first.1, second.1
                        ),
                    )
                    .await;
            }
        }

        Ok(path.display().to_string())
    }

    fn response_output_path(
        &self,
        source_uri: &Url,
        request_content_type: Option<&str>,
    ) -> std::result::Result<PathBuf, String> {
        let source_path = match source_uri.to_file_path() {
            Ok(path) => path,
            Err(_) => {
                let uri_path = source_uri.path();
                if uri_path.is_empty() {
                    return Err(format!("convert source URI to file path: {source_uri}"));
                }
                PathBuf::from(uri_path)
            }
        };
        let parent = source_path
            .parent()
            .ok_or_else(|| format!("determine parent directory for {}", source_path.display()))?;

        let stem = source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("response");
        let ext = content_type_to_extension(request_content_type);
        Ok(parent.join(format!("{stem}.response.{ext}")))
    }

    fn response_open_uri(
        &self,
        source_uri: &Url,
        response_path: &std::path::Path,
    ) -> std::result::Result<Url, String> {
        if source_uri.scheme() == "file" {
            return Url::from_file_path(response_path)
                .map_err(|_| format!("convert response path to URI: {}", response_path.display()));
        }

        let mut uri = source_uri.clone();
        let mut path = response_path.to_string_lossy().replace('\\', "/");
        if !path.starts_with('/') {
            path = format!("/{path}");
        }
        uri.set_path(&path);
        uri.set_query(None);
        uri.set_fragment(None);
        Ok(uri)
    }

    async fn try_show_document(&self, uri: &Url, external: bool) -> (bool, String) {
        let show_document = self.client.show_document(ShowDocumentParams {
            uri: uri.clone(),
            external: Some(external),
            take_focus: Some(true),
            selection: None,
        });

        match tokio::time::timeout(Duration::from_millis(1000), show_document).await {
            Ok(Ok(true)) => (true, format!("opened(external={external})")),
            Ok(Ok(false)) => (false, format!("declined(external={external})")),
            Ok(Err(e)) => (false, format!("error(external={external}): {e}")),
            Err(_) => (false, format!("timeout(external={external})")),
        }
    }

    async fn refresh_settings(&self) {
        let config_item = ConfigurationItem {
            scope_uri: None,
            section: Some("rest-client".to_string()),
        };

        match self.client.configuration(vec![config_item]).await {
            Ok(configs) => {
                let json = Value::Array(configs);
                let settings = environments::parse_settings(&json);

                let env_name = {
                    let mut state = self.state.write().await;
                    state.settings = settings;
                    self.apply_environment_variables(&mut state);
                    state
                        .settings
                        .active_environment
                        .clone()
                        .unwrap_or_else(|| "none".to_string())
                };

                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("Loaded settings, active environment: {env_name}"),
                    )
                    .await;
            }
            Err(e) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("Failed to read settings: {e}"),
                    )
                    .await;
            }
        }
    }

    fn apply_environment_variables(&self, state: &mut State) {
        let env_vars = state.settings.resolved_variables();
        for (k, v) in env_vars {
            state.variable_ctx.variables.insert(k, v);
        }
        state.variable_ctx.allowed_env_vars = state.settings.allowed_process_env_vars.clone();
    }

    fn sync_file_variables(&self, state: &mut State, text: &str) {
        let file = parser::parse(text);
        for (k, v) in file.variables {
            state.variable_ctx.variables.insert(k, v);
        }
    }

    async fn publish_diagnostics(&self, uri: &Url, text: &str) {
        let diags = handler::diagnostics(text);
        self.client
            .publish_diagnostics(uri.clone(), diags, None)
            .await;
    }
}

fn content_type_to_extension(content_type: Option<&str>) -> &'static str {
    let Some(content_type) = content_type else {
        return "txt";
    };
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    match mime.as_str() {
        "application/json" | "application/ld+json" | "application/problem+json" => "json",
        "application/xml" | "text/xml" => "xml",
        "text/html" => "html",
        "text/css" => "css",
        "text/csv" => "csv",
        "text/plain" => "txt",
        "application/x-www-form-urlencoded" => "txt",
        "application/javascript" | "text/javascript" => "js",
        "application/typescript" | "text/typescript" => "ts",
        "application/sql" | "text/sql" => "sql",
        "application/x-yaml" | "text/yaml" | "text/x-yaml" => "yaml",
        "application/pdf" => "pdf",
        _ if mime.ends_with("+json") => "json",
        _ if mime.ends_with("+xml") => "xml",
        _ if mime.starts_with("text/") => "txt",
        _ => "txt",
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        // rest-client-lsp --exec <file> <line>
        Some("--exec") if args.len() >= 4 => {
            let file = &args[2];
            let line: usize = args[3].parse().unwrap_or(1);
            std::process::exit(exec_request(file, line).await);
        }
        // rest-client-lsp --to-curl <file> <line>
        Some("--to-curl") if args.len() >= 4 => {
            let file = &args[2];
            let line: usize = args[3].parse().unwrap_or(1);
            std::process::exit(cli_to_curl(file, line));
        }
        // rest-client-lsp --from-curl <curl_command>
        Some("--from-curl") if args.len() >= 3 => {
            let curl_cmd = args[2..].join(" ");
            std::process::exit(cli_from_curl(&curl_cmd));
        }
        // rest-client-lsp --generate <language> <file> <line>
        Some("--generate") if args.len() >= 5 => {
            let language = &args[2];
            let file = &args[3];
            let line: usize = args[4].parse().unwrap_or(1);
            std::process::exit(cli_generate(language, file, line));
        }
        // rest-client-lsp --history <file>
        Some("--history") if args.len() >= 3 => {
            let file = &args[2];
            std::process::exit(cli_history(file));
        }
        _ => {}
    }

    // LSP mode (default)
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let state = Arc::new(RwLock::new(State::new()));

    let (service, socket) = LspService::new(|client| RestClientLsp {
        client,
        state: state.clone(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

async fn exec_request(file: &str, line: usize) -> i32 {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error reading {file}: {e}");
            return 1;
        }
    };

    let parsed = parser::parse(&text);
    let mut ctx = variables::VariableContext::new(parsed.variables.clone());

    let mut request = match parser::find_request_at_line(&parsed, line) {
        Some(r) => r.clone(),
        None => {
            eprintln!("No request found at line {line}");
            return 1;
        }
    };

    // Process auth headers (auto-encode Basic auth)
    auth::process_auth_headers(&mut request.headers);

    // Load session-scoped named responses.
    // These are volatile: cleared when editor closes or extension reloads.
    // The LSP manages the session file lifecycle.
    let cache_dir = std::env::temp_dir().join("rest-client-zed");
    let _ = std::fs::create_dir_all(&cache_dir);
    let file_hash = format!("{:x}", md5_hash(file));
    let cache_path = cache_dir.join(format!("{file_hash}.responses.json"));
    load_response_cache(&cache_path, &mut ctx);

    // Check for unresolved dependencies and warn
    let missing = find_missing_dependencies(&request, &ctx);
    if !missing.is_empty() {
        for name in &missing {
            eprintln!(
                "\x1b[33m⚠ Variable '{name}' has no cached response. \
                 Run the '# @name {name}' request first.\x1b[0m"
            );
        }
    }

    let resolved_url = variables::resolve(&request.url, &ctx);
    println!("# {} {}", request.method, resolved_url);
    println!();

    if let Some(note) = &request.note {
        eprintln!("\x1b[33m⚠ {note}\x1b[0m");
        eprint!("Proceed? [y/N] ");
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            let answer = input.trim().to_lowercase();
            if answer != "y" && answer != "yes" {
                println!("Request cancelled.");
                return 0;
            }
        }
    }

    match executor::execute(&request, &ctx).await {
        Ok(response) => {
            println!("HTTP {} {}", response.status, response.status_text);
            println!("Time: {}ms", response.elapsed_ms);
            println!();
            for (name, value) in &response.headers {
                println!("{name}: {value}");
            }
            println!();

            // Pretty-print JSON bodies
            let content_type = response
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map(|(_, v)| v.as_str())
                .unwrap_or("");

            if content_type.contains("json") {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&response.body) {
                    if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                        println!("{pretty}");
                    } else {
                        print!("{}", response.body);
                    }
                } else {
                    print!("{}", response.body);
                }
            } else {
                print!("{}", response.body);
            }

            // Record in history
            let history_path = cache_dir.join(format!("{file_hash}.history.json"));
            let mut hist = history::load(&history_path);
            hist.add(history::HistoryEntry {
                method: request.method.clone(),
                url: resolved_url.clone(),
                status: response.status,
                elapsed_ms: response.elapsed_ms,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            });
            history::save(&history_path, &hist);

            // Store named response for chaining (persisted to disk, scoped to file)
            if let Some(name) = &request.name {
                ctx.store_response(
                    name,
                    variables::NamedResponse {
                        headers: response.headers,
                        body: response.body,
                    },
                );
                save_response_cache(&cache_path, &ctx);
                eprintln!(
                    "\x1b[32m✓ Stored response as '{name}' — \
                     other requests can now use {{{{{name}.response.body.$.<path>}}}}\x1b[0m"
                );
            }

            0
        }
        Err(e) => {
            eprintln!("Request failed: {e}");
            1
        }
    }
}

/// Find {{name.response...}} references that have no cached response yet.
fn find_missing_dependencies(
    request: &parser::ParsedRequest,
    ctx: &variables::VariableContext,
) -> Vec<String> {
    let mut missing = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut texts = vec![request.url.clone()];
    for (_, v) in &request.headers {
        texts.push(v.clone());
    }
    if let Some(body) = &request.body {
        texts.push(body.clone());
    }

    let all_text = texts.join(" ");
    let mut rest = all_text.as_str();
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        if let Some(end) = after.find("}}") {
            let expr = after[..end].trim();
            if expr.contains(".response.") || expr.contains(".request.") {
                let dep_name = expr.split('.').next().unwrap_or("");
                if !dep_name.is_empty()
                    && !dep_name.starts_with('$')
                    && !seen.contains(dep_name)
                    && !ctx.named_responses.contains_key(dep_name)
                {
                    seen.insert(dep_name.to_string());
                    missing.push(dep_name.to_string());
                }
            }
            rest = &after[end + 2..];
        } else {
            break;
        }
    }

    missing
}

fn load_response_cache(path: &std::path::Path, ctx: &mut variables::VariableContext) {
    if let Ok(data) = std::fs::read_to_string(path) {
        if let Ok(map) =
            serde_json::from_str::<std::collections::HashMap<String, CachedResponse>>(&data)
        {
            for (name, cached) in map {
                ctx.store_response(
                    &name,
                    variables::NamedResponse {
                        headers: cached.headers,
                        body: cached.body,
                    },
                );
            }
        }
    }
}

fn save_response_cache(path: &std::path::Path, ctx: &variables::VariableContext) {
    let mut map = std::collections::HashMap::new();
    for (name, resp) in &ctx.named_responses {
        map.insert(
            name.clone(),
            CachedResponse {
                headers: resp.headers.clone(),
                body: resp.body.clone(),
            },
        );
    }
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(path, json);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedResponse {
    headers: Vec<(String, String)>,
    body: String,
}

fn clear_session_cache() {
    let cache_dir = std::env::temp_dir().join("rest-client-zed");
    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn md5_hash(input: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

fn cli_to_curl(file: &str, line: usize) -> i32 {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error reading {file}: {e}");
            return 1;
        }
    };
    let parsed = parser::parse(&text);
    let ctx = variables::VariableContext::new(parsed.variables.clone());
    let mut request = match parser::find_request_at_line(&parsed, line) {
        Some(r) => r.clone(),
        None => {
            eprintln!("No request found at line {line}");
            return 1;
        }
    };
    // Resolve variables in the request before converting
    request.url = variables::resolve(&request.url, &ctx);
    for (_, value) in &mut request.headers {
        *value = variables::resolve(value, &ctx);
    }
    if let Some(body) = &mut request.body {
        *body = variables::resolve(body, &ctx);
    }
    auth::process_auth_headers(&mut request.headers);
    println!("{}", curl::to_curl(&request));
    0
}

fn cli_from_curl(curl_cmd: &str) -> i32 {
    match curl::from_curl(curl_cmd) {
        Ok(http) => {
            print!("{http}");
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

fn cli_generate(language: &str, file: &str, line: usize) -> i32 {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error reading {file}: {e}");
            return 1;
        }
    };
    let parsed = parser::parse(&text);
    let ctx = variables::VariableContext::new(parsed.variables.clone());
    let mut request = match parser::find_request_at_line(&parsed, line) {
        Some(r) => r.clone(),
        None => {
            eprintln!("No request found at line {line}");
            return 1;
        }
    };
    request.url = variables::resolve(&request.url, &ctx);
    for (_, value) in &mut request.headers {
        *value = variables::resolve(value, &ctx);
    }
    if let Some(body) = &mut request.body {
        *body = variables::resolve(body, &ctx);
    }
    auth::process_auth_headers(&mut request.headers);
    match codegen::generate(&request, language) {
        Ok(code) => {
            print!("{code}");
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

fn cli_history(file: &str) -> i32 {
    let cache_dir = std::env::temp_dir().join("rest-client-zed");
    let file_hash = format!("{:x}", md5_hash(file));
    let history_path = cache_dir.join(format!("{file_hash}.history.json"));
    let hist = history::load(&history_path);
    print!("{}", hist.format());
    0
}
