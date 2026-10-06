//! The MCP (JSON-RPC 2.0) side of the IDE server: the handshake, the tool
//! list, and the tools the CLI calls.

use crate::{
    ClaudeCode, ConnectionId, file_uri, open_file, path_from_uri,
    review::{CloseTabArgs, OpenDiffArgs},
    workspace_folders,
};
use anyhow::{Context as _, Result, anyhow};
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{App, AppContext as _, Context, Entity, Task};
use language::{Buffer, DiagnosticSeverity, PointUtf16};
use project::Project;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use workspace::AppState;

/// Offered when the client doesn't say which version it speaks.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const PARSE_ERROR: i64 = -32700;

/// Diagnostics for the whole project stop after this many files.
const MAX_DIAGNOSTIC_FILES: usize = 200;

#[derive(Deserialize)]
struct IncomingMessage {
    id: Option<Value>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenFileArgs {
    file_path: String,
    start_text: Option<String>,
    end_text: Option<String>,
    make_frontmost: Option<bool>,
}

#[derive(Deserialize)]
struct DiagnosticsArgs {
    uri: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileArgs {
    file_path: String,
}

struct RpcError {
    code: i64,
    message: String,
}

pub(crate) fn text_content(text: impl Into<String>) -> Value {
    json!({ "type": "text", "text": text.into() })
}

fn json_content(value: Value) -> Vec<Value> {
    vec![text_content(value.to_string())]
}

fn parse_args<T: DeserializeOwned>(arguments: Value) -> Result<T> {
    serde_json::from_value(arguments).context("invalid tool arguments")
}

impl ClaudeCode {
    pub(crate) fn handle_message(
        &mut self,
        connection_id: ConnectionId,
        message: String,
        cx: &mut Context<Self>,
    ) {
        let message = match serde_json::from_str::<IncomingMessage>(&message) {
            Ok(message) => message,
            Err(error) => {
                self.send(
                    connection_id,
                    json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": PARSE_ERROR, "message": error.to_string() },
                    }),
                );
                return;
            }
        };
        let Some(method) = message.method else {
            // A response; Zed sends the CLI no requests.
            return;
        };
        let Some(id) = message.id else {
            if method == "ide_connected" {
                log::info!("Claude Code connected: {}", message.params);
            }
            return;
        };

        let response = self.handle_request(connection_id, &method, message.params, cx);
        cx.spawn(async move |this, cx| {
            let message = match response.await {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(error) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": error.code, "message": error.message },
                }),
            };
            this.update(cx, |this, _| this.send(connection_id, message))
                .ok();
        })
        .detach();
    }

    fn handle_request(
        &mut self,
        connection_id: ConnectionId,
        method: &str,
        params: Value,
        cx: &mut Context<Self>,
    ) -> Task<Result<Value, RpcError>> {
        match method {
            "initialize" => {
                let protocol_version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_PROTOCOL_VERSION);
                let version = release_channel::AppVersion::global(cx).to_string();
                Task::ready(Ok(json!({
                    "protocolVersion": protocol_version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "zed", "version": version },
                })))
            }
            "ping" => Task::ready(Ok(json!({}))),
            "tools/list" => Task::ready(Ok(json!({ "tools": tool_definitions() }))),
            "tools/call" => {
                let params = match serde_json::from_value::<ToolCallParams>(params) {
                    Ok(params) => params,
                    Err(error) => {
                        return Task::ready(Err(RpcError {
                            code: INVALID_PARAMS,
                            message: error.to_string(),
                        }));
                    }
                };
                let call = self.call_tool(connection_id, &params.name, params.arguments, cx);
                cx.background_spawn(async move {
                    Ok(match call.await {
                        Ok(content) => json!({ "content": content }),
                        Err(error) => json!({
                            "content": [text_content(format!("{error:#}"))],
                            "isError": true,
                        }),
                    })
                })
            }
            _ => Task::ready(Err(RpcError {
                code: METHOD_NOT_FOUND,
                message: format!("method not found: {method}"),
            })),
        }
    }

    fn call_tool(
        &mut self,
        connection_id: ConnectionId,
        name: &str,
        arguments: Value,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<Value>>> {
        match name {
            "openDiff" => match parse_args::<OpenDiffArgs>(arguments) {
                Ok(args) => self.open_diff(connection_id, args, cx),
                Err(error) => Task::ready(Err(error)),
            },
            "close_tab" => Task::ready(
                parse_args::<CloseTabArgs>(arguments)
                    .map(|args| self.close_tab(connection_id, args, cx)),
            ),
            "closeAllDiffTabs" => Task::ready(Ok(self.close_all_diff_tabs(connection_id, cx))),
            "openFile" => match parse_args::<OpenFileArgs>(arguments) {
                Ok(args) => self.open_file_tool(args, cx),
                Err(error) => Task::ready(Err(error)),
            },
            "getDiagnostics" => match parse_args::<DiagnosticsArgs>(arguments) {
                Ok(args) => diagnostics(args.uri.as_deref().map(path_from_uri), cx),
                Err(error) => Task::ready(Err(error)),
            },
            "getCurrentSelection" | "getLatestSelection" => {
                let result = match self.latest_selection() {
                    Some(selection) => {
                        let mut result = serde_json::to_value(selection).unwrap_or_default();
                        result["success"] = json!(true);
                        result
                    }
                    None => json!({ "success": false, "message": "No active editor found" }),
                };
                Task::ready(Ok(json_content(result)))
            }
            "getOpenEditors" => Task::ready(Ok(json_content(open_editors(cx)))),
            "getWorkspaceFolders" => {
                let folders = workspace_folders(cx);
                let root_path = folders.first().map(|folder| folder.to_string_lossy());
                let folders = folders
                    .iter()
                    .map(|folder| {
                        json!({
                            "name": folder.file_name().map(|name| name.to_string_lossy()),
                            "uri": file_uri(folder),
                            "path": folder.to_string_lossy(),
                        })
                    })
                    .collect::<Vec<_>>();
                Task::ready(Ok(json_content(json!({
                    "success": true,
                    "folders": folders,
                    "rootPath": root_path,
                }))))
            }
            "checkDocumentDirty" => Task::ready(
                parse_args::<FileArgs>(arguments).map(|args| {
                    let result = match open_buffer_for_path(Path::new(&args.file_path), cx) {
                        Some((_, buffer)) => {
                            let buffer = buffer.read(cx);
                            json!({
                                "success": true,
                                "filePath": args.file_path,
                                "isDirty": buffer.is_dirty(),
                                "isUntitled": buffer.file().is_none(),
                            })
                        }
                        None => json!({
                            "success": false,
                            "message": format!("Document not open: {}", args.file_path),
                        }),
                    };
                    json_content(result)
                }),
            ),
            "saveDocument" => match parse_args::<FileArgs>(arguments) {
                Ok(args) => save_document(args.file_path, cx),
                Err(error) => Task::ready(Err(error)),
            },
            _ => Task::ready(Err(anyhow!("unknown tool: {name}"))),
        }
    }

    fn open_file_tool(&mut self, args: OpenFileArgs, cx: &mut Context<Self>) -> Task<Result<Vec<Value>>> {
        let abs_path = PathBuf::from(&args.file_path);
        let Some((window, workspace)) = self.workspace_for_path(&abs_path, cx) else {
            return Task::ready(Err(anyhow!("no Zed window is open")));
        };
        cx.spawn(async move |_, cx| {
            let open = window.update(cx, |_, window, cx| {
                if args.make_frontmost == Some(true) {
                    window.activate_window();
                }
                workspace.update(cx, |workspace, cx| {
                    open_file(workspace, abs_path.clone(), None, true, window, cx)
                })
            })?;
            let item = open.await?;
            if let Some(start_text) = args.start_text.filter(|text| !text.is_empty())
                && let Some(editor) = item.downcast::<Editor>()
            {
                window.update(cx, |_, window, cx| {
                    editor.update(cx, |editor, cx| {
                        select_text(editor, &start_text, args.end_text.as_deref(), window, cx)
                    })
                })?;
            }
            Ok(vec![text_content(format!("Opened file: {}", args.file_path))])
        })
    }
}

/// Selects from the first occurrence of `start_text` through the end of the
/// following occurrence of `end_text`.
fn select_text(
    editor: &mut Editor,
    start_text: &str,
    end_text: Option<&str>,
    window: &mut gpui::Window,
    cx: &mut Context<Editor>,
) {
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let buffer = buffer.read(cx);
    let text = buffer.text();
    let Some(start) = text.find(start_text) else {
        return;
    };
    let end = end_text
        .filter(|end_text| !end_text.is_empty())
        .and_then(|end_text| {
            let search_from = start + start_text.len();
            text[search_from..]
                .find(end_text)
                .map(|offset| search_from + offset + end_text.len())
        })
        .unwrap_or(start + start_text.len());
    let start = buffer.offset_to_point(start);
    let end = buffer.offset_to_point(end);
    editor.change_selections(
        SelectionEffects::scroll(Autoscroll::center()),
        window,
        cx,
        |selections| selections.select_ranges([start..end]),
    );
}

fn local_projects(cx: &App) -> Vec<Entity<Project>> {
    let Some(app_state) = AppState::try_global(cx) else {
        return Vec::new();
    };
    app_state
        .workspace_store
        .read(cx)
        .workspaces()
        .filter_map(|workspace| workspace.upgrade())
        .map(|workspace| workspace.read(cx).project().clone())
        .filter(|project| project.read(cx).is_local())
        .collect()
}

fn open_buffer_for_path(abs_path: &Path, cx: &App) -> Option<(Entity<Project>, Entity<Buffer>)> {
    local_projects(cx).into_iter().find_map(|project| {
        let project_path = project.read(cx).find_project_path(abs_path, cx)?;
        let buffer = project.read(cx).get_open_buffer(&project_path, cx)?;
        Some((project, buffer))
    })
}

fn save_document(file_path: String, cx: &mut App) -> Task<Result<Vec<Value>>> {
    let Some((project, buffer)) = open_buffer_for_path(Path::new(&file_path), cx) else {
        return Task::ready(Ok(json_content(json!({
            "success": false,
            "message": format!("Document not open: {file_path}"),
        }))));
    };
    let save = project.update(cx, |project, cx| project.save_buffer(buffer, cx));
    cx.background_spawn(async move {
        let result = match save.await {
            Ok(()) => json!({
                "success": true,
                "filePath": file_path,
                "saved": true,
                "message": "Document saved successfully",
            }),
            Err(error) => json!({
                "success": false,
                "filePath": file_path,
                "saved": false,
                "message": format!("{error:#}"),
            }),
        };
        Ok(json_content(result))
    })
}

fn open_editors(cx: &App) -> Value {
    let Some(app_state) = AppState::try_global(cx) else {
        return json!({ "tabs": [] });
    };
    let active_window = cx.active_window();
    let mut tabs = Vec::new();
    for (window, workspace) in app_state.workspace_store.read(cx).workspaces_with_windows() {
        let Some(workspace) = workspace.upgrade() else {
            continue;
        };
        let workspace = workspace.read(cx);
        let active_item = workspace.active_item(cx).map(|item| item.item_id());
        for editor in workspace.items_of_type::<Editor>(cx) {
            let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
                continue;
            };
            let buffer = buffer.read(cx);
            let Some(abs_path) = buffer
                .file()
                .and_then(|file| file.as_local())
                .map(|file| file.abs_path(cx))
            else {
                continue;
            };
            tabs.push(json!({
                "uri": file_uri(&abs_path),
                "isActive": Some(window) == active_window
                    && active_item == Some(editor.entity_id()),
                "label": abs_path.file_name().map(|name| name.to_string_lossy()),
                "languageId": buffer
                    .language()
                    .map(|language| language.name().as_ref().to_lowercase()),
                "isDirty": buffer.is_dirty(),
            }));
        }
    }
    json!({ "tabs": tabs })
}

/// Diagnostics for one file, or every file that has some.
fn diagnostics(abs_path: Option<PathBuf>, cx: &mut App) -> Task<Result<Vec<Value>>> {
    let projects = local_projects(cx);
    let mut buffers = Vec::new();
    match &abs_path {
        Some(abs_path) => {
            if let Some(project) = projects
                .iter()
                .find(|project| project.read(cx).find_project_path(abs_path, cx).is_some())
                && let Some(project_path) = project.read(cx).find_project_path(abs_path, cx)
            {
                buffers.push(project.update(cx, |project, cx| project.open_buffer(project_path, cx)));
            }
        }
        None => {
            for project in &projects {
                let paths = project
                    .read(cx)
                    .diagnostic_summaries(false, cx)
                    .map(|(project_path, _, _)| project_path)
                    .collect::<Vec<_>>();
                for project_path in paths {
                    if buffers.len() >= MAX_DIAGNOSTIC_FILES {
                        break;
                    }
                    buffers.push(project.update(cx, |project, cx| project.open_buffer(project_path, cx)));
                }
            }
        }
    }

    cx.spawn(async move |cx| {
        let mut files = Vec::new();
        for buffer in buffers {
            let Ok(buffer) = buffer.await else {
                continue;
            };
            let file = buffer.read_with(cx, |buffer, cx| buffer_diagnostics(buffer, cx));
            if let Some(file) = file {
                files.push(file);
            }
        }
        // Report the file asked about even when it is clean.
        if files.is_empty()
            && let Some(abs_path) = abs_path
        {
            files.push(json!({ "uri": file_uri(&abs_path), "diagnostics": [] }));
        }
        Ok(json_content(Value::Array(files)))
    })
}

fn buffer_diagnostics(buffer: &Buffer, cx: &App) -> Option<Value> {
    let abs_path = buffer.file()?.as_local()?.abs_path(cx);
    let snapshot = buffer.snapshot();
    let diagnostics = snapshot
        .diagnostics_in_range::<_, PointUtf16>(0..snapshot.len(), false)
        .map(|entry| {
            let diagnostic = entry.diagnostic;
            let severity = match diagnostic.severity {
                DiagnosticSeverity::ERROR => "Error",
                DiagnosticSeverity::WARNING => "Warning",
                DiagnosticSeverity::INFORMATION => "Info",
                _ => "Hint",
            };
            let code = diagnostic.code.as_ref().map(|code| match code {
                lsp::NumberOrString::Number(number) => json!(number),
                lsp::NumberOrString::String(string) => json!(string),
            });
            json!({
                "message": diagnostic.message.as_str(),
                "severity": severity,
                "source": diagnostic.source,
                "code": code,
                "range": {
                    "start": { "line": entry.range.start.row, "character": entry.range.start.column },
                    "end": { "line": entry.range.end.row, "character": entry.range.end.column },
                },
            })
        })
        .collect::<Vec<_>>();
    Some(json!({ "uri": file_uri(&abs_path), "diagnostics": diagnostics }))
}

fn tool_definitions() -> Vec<Value> {
    fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
            },
        })
    }
    let file_path = json!({ "filePath": { "type": "string", "description": "Absolute path of the file" } });

    vec![
        tool(
            "openDiff",
            "Show a proposed change to a file inline in Zed and wait until the user has accepted or rejected each part of it",
            json!({
                "old_file_path": { "type": "string", "description": "Absolute path of the file before the change" },
                "new_file_path": { "type": "string", "description": "Absolute path of the file after the change" },
                "new_file_contents": { "type": "string", "description": "The proposed contents of the file" },
                "tab_name": { "type": "string", "description": "A name that identifies this proposal" },
            }),
            &["old_file_path", "new_file_path", "new_file_contents", "tab_name"],
        ),
        tool(
            "close_tab",
            "Withdraw a proposed change that is waiting for review",
            json!({ "tab_name": { "type": "string" } }),
            &["tab_name"],
        ),
        tool("closeAllDiffTabs", "Withdraw every proposed change waiting for review", json!({}), &[]),
        tool(
            "openFile",
            "Open a file in Zed, optionally selecting a range of its text",
            json!({
                "filePath": { "type": "string", "description": "Absolute path of the file" },
                "preview": { "type": "boolean" },
                "startText": { "type": "string", "description": "Text the selection starts at" },
                "endText": { "type": "string", "description": "Text the selection ends with" },
                "selectToEndOfLine": { "type": "boolean" },
                "makeFrontmost": { "type": "boolean" },
            }),
            &["filePath"],
        ),
        tool(
            "getDiagnostics",
            "Get language server diagnostics from Zed for one file, or for every file that has some",
            json!({ "uri": { "type": "string", "description": "file:// URI of the file; omit for all files" } }),
            &[],
        ),
        tool("getCurrentSelection", "Get the text selected in Zed's active editor", json!({}), &[]),
        tool("getLatestSelection", "Get the most recent text selection in Zed", json!({}), &[]),
        tool("getOpenEditors", "List the files open in Zed", json!({}), &[]),
        tool("getWorkspaceFolders", "List the project folders open in Zed", json!({}), &[]),
        tool("checkDocumentDirty", "Check whether a file open in Zed has unsaved changes", file_path.clone(), &["filePath"]),
        tool("saveDocument", "Save a file open in Zed", file_path, &["filePath"]),
    ]
}
