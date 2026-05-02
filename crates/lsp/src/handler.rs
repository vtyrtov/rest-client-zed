use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::*;

use crate::environments::RestClientSettings;
use crate::executor;
use crate::formatter;
use crate::parser;
use crate::variables::{self, NamedResponse, VariableContext};

pub const SEND_REQUEST_COMMAND: &str = "rest-client.sendRequest";

pub struct State {
    pub documents: HashMap<Url, String>,
    pub variable_ctx: VariableContext,
    pub settings: RestClientSettings,
}

impl State {
    pub fn new() -> Self {
        Self {
            documents: HashMap::new(),
            variable_ctx: VariableContext::new(HashMap::new()),
            settings: RestClientSettings::default(),
        }
    }
}

pub type SharedState = Arc<RwLock<State>>;

pub struct ExecuteRequestOutput {
    pub formatted_response: String,
    pub file_content: String,
    pub request_content_type: Option<String>,
}

pub fn code_lenses(uri: &Url, text: &str) -> Vec<CodeLens> {
    let file = parser::parse(text);
    file.requests
        .iter()
        .map(|req| {
            let title = format!("Send Request - {} {}", req.method, req.url);
            CodeLens {
                range: Range {
                    start: Position {
                        line: req.line as u32,
                        character: 0,
                    },
                    end: Position {
                        line: req.line as u32,
                        character: 0,
                    },
                },
                command: Some(Command {
                    title,
                    command: SEND_REQUEST_COMMAND.to_string(),
                    arguments: Some(vec![
                        Value::String(uri.to_string()),
                        Value::Number(req.line.into()),
                    ]),
                }),
                data: None,
            }
        })
        .collect()
}

pub fn diagnostics(text: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let file = parser::parse(text);

    for req in &file.requests {
        if req.url.is_empty() {
            diags.push(Diagnostic {
                range: Range {
                    start: Position {
                        line: req.line as u32,
                        character: 0,
                    },
                    end: Position {
                        line: req.line as u32,
                        character: lines.get(req.line).map(|l| l.len() as u32).unwrap_or(0),
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("rest-client".to_string()),
                message: "Request URL is empty".to_string(),
                ..Default::default()
            });
        }
    }

    let _ = (lines, file);

    diags
}

/// Find the definition location of a variable or named request under the cursor.
pub fn goto_definition(uri: &Url, text: &str, position: Position) -> Option<Location> {
    let var_name = extract_variable_at(text, position)?;
    let lines: Vec<&str> = text.lines().collect();

    // Check for named request reference (e.g., "login" from "login.response.body...")
    let base_name = var_name.split('.').next().unwrap_or(&var_name);

    // Look for @name annotation
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let rest = rest.trim();
            if let Some(rest) = rest.strip_prefix("@name") {
                let name = rest.trim();
                if name == base_name {
                    return Some(Location {
                        uri: uri.clone(),
                        range: Range {
                            start: Position {
                                line: i as u32,
                                character: 0,
                            },
                            end: Position {
                                line: i as u32,
                                character: line.len() as u32,
                            },
                        },
                    });
                }
            }
        }
    }

    // Look for file variable (@var = value)
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('@') {
            if let Some(eq_pos) = rest.find('=') {
                let name = rest[..eq_pos].trim();
                if name == base_name {
                    return Some(Location {
                        uri: uri.clone(),
                        range: Range {
                            start: Position {
                                line: i as u32,
                                character: 0,
                            },
                            end: Position {
                                line: i as u32,
                                character: line.len() as u32,
                            },
                        },
                    });
                }
            }
        }
    }

    None
}

/// Show the resolved value of a variable on hover.
pub fn hover_at(text: &str, position: Position, ctx: &VariableContext) -> Option<Hover> {
    let var_name = extract_variable_at(text, position)?;
    let resolved = variables::resolve(&format!("{{{{{var_name}}}}}"), ctx);

    let display = if resolved.is_empty() {
        format!("`{var_name}` — *undefined*")
    } else {
        format!("`{var_name}` = `{resolved}`")
    };

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: display,
        }),
        range: None,
    })
}

/// Extract the variable name under the cursor if inside {{ }}.
fn extract_variable_at(text: &str, position: Position) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let line_idx = position.line as usize;
    if line_idx >= lines.len() {
        return None;
    }

    let line = lines[line_idx];
    let col = position.character as usize;

    // Find {{ before cursor
    let before = if col <= line.len() {
        &line[..col]
    } else {
        line
    };
    let open = before.rfind("{{")?;
    let after_open = &line[open + 2..];

    // Find }} after the opening
    let close = after_open.find("}}")?;
    let var_name = after_open[..close].trim();

    if var_name.is_empty() {
        return None;
    }

    // Strip $ prefix for system variables (not navigable)
    if var_name.starts_with('$') {
        return None;
    }

    Some(var_name.to_string())
}

pub fn completions_at(
    text: &str,
    position: Position,
    ctx: &VariableContext,
) -> Vec<CompletionItem> {
    let lines: Vec<&str> = text.lines().collect();
    let line_idx = position.line as usize;
    if line_idx >= lines.len() {
        return vec![];
    }

    let line = lines[line_idx];
    let col = position.character as usize;
    let before_cursor = if col <= line.len() {
        &line[..col]
    } else {
        line
    };

    // Check if we're inside {{ }}
    if let Some(open) = before_cursor.rfind("{{") {
        let after_open = &before_cursor[open + 2..];
        if !after_open.contains("}}") {
            // We're inside a variable reference
            let prefix = after_open.trim();
            let vars = variables::available_variables(ctx);
            return vars
                .into_iter()
                .filter(|v| prefix.is_empty() || v.starts_with(prefix))
                .map(|name| CompletionItem {
                    label: name.clone(),
                    kind: Some(if name.starts_with('$') {
                        CompletionItemKind::FUNCTION
                    } else {
                        CompletionItemKind::VARIABLE
                    }),
                    detail: Some("variable".to_string()),
                    ..Default::default()
                })
                .collect();
        }
    }

    vec![]
}

pub async fn execute_request(
    uri: &Url,
    line: usize,
    state: &SharedState,
) -> Result<ExecuteRequestOutput, String> {
    let (text, mut ctx) = {
        let state = state.read().await;
        let text = state
            .documents
            .get(uri)
            .cloned()
            .ok_or_else(|| "document not found".to_string())?;
        let ctx = VariableContext::new(state.variable_ctx.variables.clone());
        (text, ctx)
    };

    let file = parser::parse(&text);

    // Merge file variables into context
    for (k, v) in &file.variables {
        ctx.variables.insert(k.clone(), v.clone());
    }

    // Copy named responses from shared state
    {
        let state = state.read().await;
        for (k, v) in &state.variable_ctx.named_responses {
            ctx.named_responses.insert(
                k.clone(),
                NamedResponse {
                    headers: v.headers.clone(),
                    body: v.body.clone(),
                },
            );
        }
    }

    let request = parser::find_request_at_line(&file, line)
        .ok_or_else(|| format!("no request found at line {line}"))?;

    let request_content_type = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone());

    let response = executor::execute(request, &ctx).await?;
    let formatted = formatter::format_response(&response);
    let file_content = if response.status == 200 {
        formatter::format_response_body(&response.body, &response.headers)
    } else {
        formatter::format_response_diagnostics(&response)
    };

    // Store named response if request has a name
    if let Some(name) = &request.name {
        let mut state = state.write().await;
        state.variable_ctx.store_response(
            name,
            NamedResponse {
                headers: response.headers.clone(),
                body: response.body.clone(),
            },
        );
    }

    Ok(ExecuteRequestOutput {
        formatted_response: formatted,
        file_content,
        request_content_type,
    })
}
