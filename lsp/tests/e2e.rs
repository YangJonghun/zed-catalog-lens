use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_secs(1);
const BINARY: &str = env!("CARGO_BIN_EXE_catalog-lens");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .canonicalize()
        .unwrap()
}

fn uri_for(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn position_of(text: &str, needle: &str, nth: usize) -> Value {
    let offset = text.match_indices(needle).nth(nth).map(|(i, _)| i).unwrap();
    let line = text[..offset].matches('\n').count();
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let character = text[line_start..offset].encode_utf16().count();
    json!({ "line": line, "character": character })
}

fn end_of(text: &str, needle: &str, nth: usize) -> Value {
    let start = position_of(text, needle, nth);
    json!({
        "line": start["line"],
        "character": start["character"].as_u64().unwrap() + needle.encode_utf16().count() as u64
    })
}

fn range_of(text: &str, needle: &str, nth: usize) -> Value {
    json!({ "start": position_of(text, needle, nth), "end": end_of(text, needle, nth) })
}

#[derive(Default)]
struct Options {
    show_document: bool,
    settings: Value,
    code_lens: CodeLens,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum CodeLens {
    #[default]
    Off,
    /// Lenses drawn above lines: `workspace/codeLens/refresh` makes every open
    /// editor ask for lenses again.
    On,
    /// Lenses in the code action menu: lens requests come with cursor moves
    /// (sent by the test) and refreshes are ignored.
    Menu,
}

#[derive(Default)]
struct Editor {
    code_lens: CodeLens,
    open: Vec<String>,
}

struct Server {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    incoming: Receiver<Value>,
    buffered: Vec<Value>,
    recorded: Arc<Mutex<Vec<Value>>>,
    editor: Arc<Mutex<Editor>>,
    hint_refreshes: Arc<AtomicUsize>,
    hint_refreshes_handled: usize,
    settings: Value,
    initialize_result: Value,
    next_id: i64,
}

impl Server {
    fn start(options: Options) -> Server {
        let mut child = Command::new(BINARY)
            .env("CATALOG_LENS_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let stdout = child.stdout.take().unwrap();
        let (sender, incoming) = channel();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let editor = Arc::new(Mutex::new(Editor {
            code_lens: options.code_lens,
            open: Vec::new(),
        }));
        let hint_refreshes = Arc::new(AtomicUsize::new(0));
        let reader = Reader {
            stdin: stdin.clone(),
            sender,
            recorded: recorded.clone(),
            editor: editor.clone(),
            hint_refreshes: hint_refreshes.clone(),
            auto_ids: AtomicI64::new(1_000_000),
        };
        thread::spawn(move || reader.run(BufReader::new(stdout)));
        let mut server = Server {
            child,
            stdin,
            incoming,
            buffered: Vec::new(),
            recorded,
            editor,
            hint_refreshes,
            hint_refreshes_handled: 0,
            settings: Value::Null,
            initialize_result: Value::Null,
            next_id: 0,
        };
        server.initialize_result = server.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": uri_for(&fixtures()),
                "capabilities": {
                    "window": { "showDocument": { "support": options.show_document } }
                }
            }),
        );
        server.notify("initialized", json!({}));
        // Zed sends `{}` when the extension returns no settings.
        server.settings = match options.settings {
            Value::Null => json!({}),
            settings => settings,
        };
        server.change_settings(server.settings.clone());
        server.wait_recorded("client/registerCapability");
        server
    }

    /// What Zed sends after `initialized` and on every settings.json edit.
    fn change_settings(&self, settings: Value) {
        self.notify(
            "workspace/didChangeConfiguration",
            json!({ "settings": settings }),
        );
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let response = self.next_matching(|message| message["id"] == json!(id));
        assert!(
            response.get("error").is_none(),
            "{method} failed: {}",
            response["error"]
        );
        response["result"].clone()
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn send(&self, message: Value) {
        write_message(&mut self.stdin.lock().unwrap(), &message);
    }

    fn wait_notification(&mut self, method: &str, matches: impl Fn(&Value) -> bool) -> Value {
        self.next_matching(|message| {
            message["method"] == json!(method)
                && message.get("id").is_none()
                && matches(&message["params"])
        })["params"]
            .clone()
    }

    fn wait_diagnostics(&mut self, uri: &str) -> Vec<Value> {
        let params = self.wait_notification("textDocument/publishDiagnostics", |params| {
            params["uri"] == json!(uri)
        });
        params["diagnostics"].as_array().unwrap().clone()
    }

    fn no_notification_within(&mut self, method: &str, wait: Duration) {
        let deadline = Instant::now() + wait;
        while let Ok(message) = self
            .incoming
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            assert_ne!(
                message["method"],
                json!(method),
                "unexpected {method}: {message}"
            );
            self.buffered.push(message);
        }
    }

    fn next_matching(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.buffered.iter().position(&matches) {
            return self.buffered.remove(index);
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.incoming.recv_timeout(remaining) {
                Ok(message) if matches(&message) => return message,
                Ok(message) => self.buffered.push(message),
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for a message"),
                Err(RecvTimeoutError::Disconnected) => panic!("server exited"),
            }
        }
    }

    fn recorded(&self, method: &str) -> Vec<Value> {
        self.recorded
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message["method"] == json!(method))
            .cloned()
            .collect()
    }

    fn wait_recorded(&self, method: &str) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(found) = self.recorded(method).into_iter().next() {
                return found;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {method}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn clear_recorded(&self) {
        self.recorded.lock().unwrap().clear();
    }

    /// What Zed does when the user edits `code_lens` in settings.json: open
    /// editors switch at once (turning lenses on fetches them), then every
    /// server gets its unchanged settings again.
    fn set_code_lens(&mut self, code_lens: CodeLens) {
        let open = {
            let mut editor = self.editor.lock().unwrap();
            editor.code_lens = code_lens;
            editor.open.clone()
        };
        if code_lens == CodeLens::On {
            for uri in &open {
                self.code_lenses(uri);
            }
        }
        self.change_settings(self.settings.clone());
    }

    fn hints_after_refreshes(&mut self, uri: &str, mut on_screen: Value) -> Value {
        loop {
            let deadline = Instant::now() + SETTLE;
            while self.hint_refreshes.load(Ordering::SeqCst) == self.hint_refreshes_handled {
                if Instant::now() >= deadline {
                    return on_screen;
                }
                thread::sleep(Duration::from_millis(10));
            }
            on_screen = self.inlay_hints(uri);
        }
    }

    fn open(&mut self, path: &Path) -> (String, String) {
        let text = std::fs::read_to_string(path).unwrap();
        let uri = uri_for(path);
        self.open_text(&uri, &text);
        (uri, text)
    }

    fn open_text(&mut self, uri: &str, text: &str) {
        self.editor.lock().unwrap().open.push(uri.to_owned());
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": "json", "version": 1, "text": text } }),
        );
    }

    fn change(&mut self, uri: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] }),
        );
    }

    fn inlay_hints(&mut self, uri: &str) -> Value {
        self.hint_refreshes_handled = self.hint_refreshes.load(Ordering::SeqCst);
        self.request(
            "textDocument/inlayHint",
            json!({ "textDocument": { "uri": uri }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 9999, "character": 0 } } }),
        )
    }

    fn code_lenses(&mut self, uri: &str) -> Value {
        self.request(
            "textDocument/codeLens",
            json!({ "textDocument": { "uri": uri } }),
        )
    }

    fn at(&mut self, method: &str, uri: &str, position: Value) -> Value {
        self.request(
            method,
            json!({ "textDocument": { "uri": uri }, "position": position }),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Reader {
    stdin: Arc<Mutex<ChildStdin>>,
    sender: Sender<Value>,
    recorded: Arc<Mutex<Vec<Value>>>,
    editor: Arc<Mutex<Editor>>,
    hint_refreshes: Arc<AtomicUsize>,
    auto_ids: AtomicI64,
}

impl Reader {
    fn run(self, mut stdout: BufReader<impl Read>) {
        while let Some(message) = read_message(&mut stdout) {
            let is_server_request = message.get("method").is_some() && message.get("id").is_some();
            if !is_server_request {
                if self.sender.send(message).is_err() {
                    return;
                }
                continue;
            }
            self.recorded.lock().unwrap().push(message.clone());
            let result = match message["method"].as_str().unwrap() {
                "window/showDocument" => json!({ "success": true }),
                _ => Value::Null,
            };
            write_message(
                &mut self.stdin.lock().unwrap(),
                &json!({ "jsonrpc": "2.0", "id": message["id"], "result": result }),
            );
            if message["method"] == json!("workspace/inlayHint/refresh") {
                self.hint_refreshes.fetch_add(1, Ordering::SeqCst);
            }
            if message["method"] == json!("workspace/codeLens/refresh") {
                let editor = self.editor.lock().unwrap();
                if editor.code_lens == CodeLens::On {
                    for uri in &editor.open {
                        let id = self.auto_ids.fetch_add(1, Ordering::SeqCst);
                        write_message(
                            &mut self.stdin.lock().unwrap(),
                            &json!({ "jsonrpc": "2.0", "id": id, "method": "textDocument/codeLens", "params": { "textDocument": { "uri": uri } } }),
                        );
                    }
                }
            }
        }
    }
}

fn write_message(stdin: &mut ChildStdin, message: &Value) {
    let body = message.to_string();
    write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    stdin.flush().unwrap();
}

fn read_message(reader: &mut BufReader<impl Read>) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" {
            break;
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            length = rest.trim().parse().ok();
        }
    }
    let mut body = vec![0; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

struct TempWorkspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl TempWorkspace {
    fn empty() -> TempWorkspace {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        TempWorkspace { _dir: dir, root }
    }

    fn copy_of(fixture: &str) -> TempWorkspace {
        let workspace = TempWorkspace::empty();
        copy_dir(&fixtures().join(fixture), &workspace.root);
        workspace
    }

    fn with_files(files: &[(&str, &str)]) -> TempWorkspace {
        let workspace = TempWorkspace::empty();
        for (relative, text) in files {
            workspace.write(relative, text);
        }
        workspace
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn hints_by_label(hints: &Value) -> HashMap<String, Value> {
    hints
        .as_array()
        .unwrap()
        .iter()
        .map(|hint| {
            (
                hint["label"][0]["value"].as_str().unwrap().to_owned(),
                hint.clone(),
            )
        })
        .collect()
}

fn messages(diagnostics: &[Value]) -> Vec<String> {
    diagnostics
        .iter()
        .map(|d| d["message"].as_str().unwrap().to_owned())
        .collect()
}

#[derive(Debug, Default, PartialEq)]
struct Seen {
    hints: BTreeMap<String, String>,
    warnings: Vec<(String, String)>,
}

fn key_on_line(text: &str, line: u64) -> String {
    let line = text.lines().nth(line as usize).unwrap_or_default();
    line.split('"').nth(1).unwrap_or_default().to_owned()
}

fn seen(temp: &TempWorkspace, text: &str) -> Seen {
    let mut server = Server::start(Options::default());
    let uri = uri_for(&temp.path("app/package.json"));
    temp.write("app/package.json", text);
    server.open_text(&uri, text);
    let diagnostics = server.wait_diagnostics(&uri);
    let hints = server.inlay_hints(&uri);
    Seen {
        hints: hints
            .as_array()
            .unwrap()
            .iter()
            .map(|hint| {
                (
                    key_on_line(text, hint["position"]["line"].as_u64().unwrap()),
                    hint["label"][0]["value"].as_str().unwrap().to_owned(),
                )
            })
            .collect(),
        warnings: diagnostics
            .iter()
            .map(|d| {
                (
                    key_on_line(text, d["range"]["start"]["line"].as_u64().unwrap()),
                    d["message"].as_str().unwrap().to_owned(),
                )
            })
            .collect(),
    }
}

fn hints(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, label)| (key.to_string(), label.to_string()))
        .collect()
}

fn warnings(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, message)| (key.to_string(), message.to_string()))
        .collect()
}

fn pnpm_app() -> PathBuf {
    fixtures().join("pnpm/packages/app/package.json")
}

#[test]
fn version_flag_prints_the_crate_version() {
    let output = Command::new(BINARY).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn inlay_hints_show_versions_after_the_value_token() {
    let mut server = Server::start(Options::default());
    let (uri, text) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let hints = server.inlay_hints(&uri);
    let hints = hints.as_array().unwrap();
    let react = hints
        .iter()
        .find(|hint| hint["position"] == end_of(&text, "\"catalog:\"", 0))
        .expect("hint for react");
    assert_eq!(react["label"][0]["value"], json!("^18.3.1"));
    assert_eq!(
        react["label"][0]["tooltip"],
        json!("catalog `default` · pnpm-workspace.yaml")
    );
    assert_eq!(react["paddingLeft"], json!(true));
    let location = &react["label"][0]["location"];
    assert_eq!(
        location["uri"],
        json!(uri_for(&fixtures().join("pnpm/pnpm-workspace.yaml")))
    );
    assert_eq!(
        react["label"][0]["command"]["command"],
        json!("editor.action.goToLocations")
    );
    let react_dom = hints
        .iter()
        .find(|hint| hint["position"] == end_of(&text, "\"catalog:react18\"", 0))
        .expect("hint for react-dom");
    assert_eq!(react_dom["label"][0]["value"], json!("^18.3.1"));
    assert_eq!(
        react_dom["label"][0]["tooltip"],
        json!("catalog `react18` · pnpm-workspace.yaml")
    );
    let positions: Vec<&Value> = hints.iter().map(|hint| &hint["position"]).collect();
    assert!(
        !positions.contains(&&end_of(&text, "\"catalog:react17\"", 0)),
        "unresolved references get no hint"
    );
    assert_eq!(
        hints.len(),
        4,
        "react, react-dom, lodash, override, and nothing else: {hints:?}"
    );
}

#[test]
fn named_catalog_label_prefixes_the_catalog_name() {
    let mut server = Server::start(Options {
        settings: json!({ "namedCatalogLabel": true }),
        ..Options::default()
    });
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let hints = hints_by_label(&server.inlay_hints(&uri));
    assert!(
        hints.contains_key("react18 → ^18.3.1"),
        "{:?}",
        hints.keys()
    );
    assert!(
        hints.contains_key("^18.3.1"),
        "default catalog keeps the bare version"
    );
}

#[test]
fn code_lens_mirrors_the_line_and_navigates_with_the_client_command() {
    let mut server = Server::start(Options {
        settings: json!({ "display": "codeLens" }),
        ..Options::default()
    });
    let (uri, text) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let lenses = server.code_lenses(&uri);
    let lens = lenses
        .as_array()
        .unwrap()
        .iter()
        .find(|lens| lens["range"] == range_of(&text, "\"react-dom\": \"catalog:react18\"", 0))
        .expect("lens for react-dom spans the whole entry, so the code action menu finds it anywhere on it");
    assert_eq!(
        lens["command"]["title"],
        json!("\"react-dom\": \"^18.3.1\"")
    );
    assert_eq!(
        lens["command"]["command"],
        json!("editor.action.goToLocations")
    );
    let arguments = lens["command"]["arguments"].as_array().unwrap();
    assert_eq!(arguments[0], json!(uri));
    assert_eq!(arguments[1], position_of(&text, "\"catalog:react18\"", 0));
    assert_eq!(
        arguments[2][0]["uri"],
        json!(uri_for(&fixtures().join("pnpm/pnpm-workspace.yaml")))
    );
    assert!(
        server.inlay_hints(&uri).as_array().unwrap().is_empty(),
        "code_lens mode hides hints"
    );
}

#[test]
fn hover_covers_the_whole_entry_from_key_to_value() {
    let mut server = Server::start(Options::default());
    let (uri, text) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let entry = "\"react-dom\": \"catalog:react18\"";
    for position in [
        position_of(&text, "catalog:react18", 0),
        position_of(&text, entry, 0),
        position_of(&text, "-dom\":", 0),
    ] {
        let hover = server.at("textDocument/hover", &uri, position.clone());
        assert_eq!(
            hover["contents"]["kind"],
            json!("markdown"),
            "at {position}"
        );
        assert_eq!(
            hover["contents"]["value"],
            json!(
                "**react-dom** · catalog `react18`\n\nVersion: `^18.3.1`\n\nDefined in: `pnpm-workspace.yaml:13`"
            )
        );
        assert_eq!(hover["range"], range_of(&text, entry, 0));
    }
    assert_eq!(
        server.at(
            "textDocument/definition",
            &uri,
            position_of(&text, entry, 0)
        ),
        Value::Null,
        "go to definition stays on the value"
    );
    let outside = server.at(
        "textDocument/hover",
        &uri,
        json!({ "line": 0, "character": 0 }),
    );
    assert_eq!(outside, Value::Null);
}

#[test]
fn definition_resolves_selector_keys_to_the_catalog_entry() {
    let mut server = Server::start(Options::default());
    let (uri, text) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let workspace_text =
        std::fs::read_to_string(fixtures().join("pnpm/pnpm-workspace.yaml")).unwrap();
    let definition = server.at(
        "textDocument/definition",
        &uri,
        end_of(&text, "\"catalog:react19\"", 0),
    );
    assert_eq!(
        definition["uri"],
        json!(uri_for(&fixtures().join("pnpm/pnpm-workspace.yaml")))
    );
    assert_eq!(definition["range"], range_of(&workspace_text, "^19.0.0", 0));
    let plain = server.at(
        "textDocument/definition",
        &uri,
        position_of(&text, "\"catalog:\"", 0),
    );
    assert_eq!(plain["range"], range_of(&workspace_text, "^18.3.1", 0));
}

#[test]
fn diagnostics_cover_missing_entries_catalogs_and_blank_names() {
    let mut server = Server::start(Options::default());
    let (uri, text) = server.open(&pnpm_app());
    let diagnostics = server.wait_diagnostics(&uri);
    assert_eq!(
        messages(&diagnostics),
        vec![
            "\"missing-pkg\" is not defined in the default catalog (pnpm-workspace.yaml)",
            "Catalog \"react17\" is not defined in pnpm-workspace.yaml",
            "\"blank-name\" is not defined in the default catalog (pnpm-workspace.yaml)",
            "\"@types/react\" is not defined in catalog \"react18\" (pnpm-workspace.yaml)",
        ]
    );
    for diagnostic in &diagnostics {
        assert_eq!(diagnostic["severity"], json!(2));
    }
    assert_eq!(diagnostics[0]["range"], range_of(&text, "\"catalog:\"", 2));
}

#[test]
fn diagnostics_report_a_workspace_file_that_does_not_parse() {
    let temp = TempWorkspace::copy_of("pnpm");
    temp.write(
        "pnpm-workspace.yaml",
        "catalog:\n  react: ^18.3.1\n - broken\n",
    );
    let mut server = Server::start(Options::default());
    let (uri, _) = server.open(&temp.path("packages/app/package.json"));
    assert_eq!(
        messages(&server.wait_diagnostics(&uri)),
        vec!["pnpm-workspace.yaml does not parse; no catalogs available"]
    );

    let definition = uri_for(&temp.path("pnpm-workspace.yaml"));
    temp.write("pnpm-workspace.yaml", "catalog:\n  react: ^18.3.1\n");
    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": definition, "type": 2 }] }),
    );
    let healthy = server.wait_diagnostics(&uri);
    assert!(
        messages(&healthy)
            .iter()
            .all(|m| m.contains("is not defined")),
        "{healthy:?}"
    );

    temp.write(
        "pnpm-workspace.yaml",
        "catalog:\n  react: ^18.3.1\n - broken\n",
    );
    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": definition, "type": 2 }] }),
    );
    let stale = server.wait_diagnostics(&uri);
    assert_eq!(
        messages(&stale)[0],
        "pnpm-workspace.yaml does not parse; showing its last good catalogs"
    );
    let hints = hints_by_label(&server.inlay_hints(&uri));
    assert!(
        hints.contains_key("^18.3.1"),
        "last good catalogs still resolve"
    );
}

#[test]
fn diagnostics_report_a_missing_workspace_once_per_document() {
    let mut server = Server::start(Options::default());
    let (uri, text) = server.open(&fixtures().join("none/package.json"));
    let diagnostics = server.wait_diagnostics(&uri);
    assert_eq!(
        messages(&diagnostics),
        vec![
            "No catalog definitions found: expected pnpm-workspace.yaml, .yarnrc.yml, or a package.json with a `catalog` field in a parent directory"
        ]
    );
    assert_eq!(diagnostics[0]["range"], range_of(&text, "\"catalog:\"", 0));
    assert!(server.inlay_hints(&uri).as_array().unwrap().is_empty());
}

#[test]
fn did_close_clears_diagnostics() {
    let mut server = Server::start(Options::default());
    let (uri, _) = server.open(&pnpm_app());
    assert!(!server.wait_diagnostics(&uri).is_empty());
    server.notify(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(server.wait_diagnostics(&uri).is_empty());
    assert_eq!(server.inlay_hints(&uri), Value::Null);
}

#[test]
fn watched_file_change_republishes_diagnostics() {
    let temp = TempWorkspace::copy_of("pnpm");
    let mut server = Server::start(Options::default());
    let (uri, _) = server.open(&temp.path("packages/app/package.json"));
    let before = server.wait_diagnostics(&uri);
    assert!(
        messages(&before)
            .iter()
            .any(|m| m.starts_with("\"missing-pkg\""))
    );

    let text = std::fs::read_to_string(temp.path("pnpm-workspace.yaml"))
        .unwrap()
        .replace("catalog:\n", "catalog:\n  missing-pkg: ^1.0.0\n");
    let definition = uri_for(&temp.path("pnpm-workspace.yaml"));
    temp.write("pnpm-workspace.yaml", &text);
    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": definition, "type": 2 }] }),
    );
    let after = server.wait_diagnostics(&uri);
    assert!(
        !messages(&after)
            .iter()
            .any(|m| m.starts_with("\"missing-pkg\"")),
        "{after:?}"
    );
}

#[test]
fn watched_file_outside_open_documents_is_ignored() {
    let mut server = Server::start(Options::default());
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": uri_for(&fixtures().join("yarn/.yarnrc.yml")), "type": 2 }] }),
    );
    server.no_notification_within(
        "textDocument/publishDiagnostics",
        Duration::from_millis(400),
    );
}

#[test]
fn unsaved_bun_root_buffer_wins_over_disk() {
    let mut server = Server::start(Options::default());
    let root = fixtures().join("bun/package.json");
    let (root_uri, root_text) = server.open(&root);
    server.wait_diagnostics(&root_uri);
    let (app_uri, app_text) = server.open(&fixtures().join("bun/packages/app/package.json"));
    server.wait_diagnostics(&app_uri);
    let hints = hints_by_label(&server.inlay_hints(&app_uri));
    assert!(
        hints.contains_key("^19.0.0") && hints.contains_key("^18.3.1"),
        "{:?}",
        hints.keys()
    );

    let edited = root_text.replace("\"react\": \"^19.0.0\"", "\"react\": \"^20.0.0\"");
    server.change(&root_uri, 2, &edited);
    server.wait_diagnostics(&app_uri);
    let hints = hints_by_label(&server.inlay_hints(&app_uri));
    assert!(hints.contains_key("^20.0.0"), "{:?}", hints.keys());
    let definition = server.at(
        "textDocument/definition",
        &app_uri,
        position_of(&app_text, "catalog: \"", 0),
    );
    assert_eq!(definition["uri"], json!(root_uri));
    assert_eq!(
        definition["range"],
        range_of(&edited, "\"^20.0.0\"", 0),
        "definition follows the buffer"
    );

    let mid_edit = edited.replace("\"react-dom\": \"^19.0.0\"", "\"react-dom\": \"^19.");
    server.change(&root_uri, 3, &mid_edit);
    server.wait_diagnostics(&app_uri);
    let hints = hints_by_label(&server.inlay_hints(&app_uri));
    assert!(
        hints.contains_key("^20.0.0"),
        "typing in the root keeps the other entries: {:?}",
        hints.keys()
    );
}

#[test]
fn stale_did_change_versions_are_ignored() {
    let mut server = Server::start(Options::default());
    let (uri, text) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    server.change(
        &uri,
        3,
        &text.replace("\"react\": \"catalog:\"", "\"react\": \"catalog:react19\""),
    );
    server.wait_diagnostics(&uri);
    server.change(&uri, 2, &text);
    server.no_notification_within(
        "textDocument/publishDiagnostics",
        Duration::from_millis(300),
    );
    let hints = hints_by_label(&server.inlay_hints(&uri));
    assert!(hints.contains_key("^19.0.0"), "{:?}", hints.keys());
}

#[test]
fn yarn_fixture_resolves_flow_sequence_keys_and_resolutions() {
    let mut server = Server::start(Options::default());
    let (uri, text) = server.open(&fixtures().join("yarn/packages/app/package.json"));
    assert!(server.wait_diagnostics(&uri).is_empty());
    let hints = server.inlay_hints(&uri);
    let hints = hints.as_array().unwrap();
    let at = |needle: &str, nth: usize| {
        hints
            .iter()
            .find(|hint| hint["position"] == end_of(&text, needle, nth))
            .map(|hint| hint["label"][0]["value"].clone())
    };
    assert_eq!(at("\"catalog:react18\"", 0), Some(json!("^18.3.1")));
    assert_eq!(at("\"catalog:\"", 0), Some(json!("^4.17.21")));
    assert_eq!(at("\"catalog:\"", 1), Some(json!("^18.3.12")));
    assert_eq!(at("\"catalog:react18\"", 1), Some(json!("^18.3.1")));
    let yarnrc = std::fs::read_to_string(fixtures().join("yarn/.yarnrc.yml")).unwrap();
    let definition = server.at(
        "textDocument/definition",
        &uri,
        position_of(&text, "\"catalog:\"", 1),
    );
    assert_eq!(
        definition["uri"],
        json!(uri_for(&fixtures().join("yarn/.yarnrc.yml")))
    );
    assert_eq!(definition["range"], range_of(&yarnrc, "^18.3.12", 0));
}

#[test]
fn bun_fixture_trims_catalog_names() {
    let mut server = Server::start(Options::default());
    let (uri, text) = server.open(&fixtures().join("bun/packages/app/package.json"));
    assert!(server.wait_diagnostics(&uri).is_empty());
    let hints = server.inlay_hints(&uri);
    let hints = hints.as_array().unwrap();
    let react = hints
        .iter()
        .find(|h| h["position"] == end_of(&text, "\"catalog: \"", 0))
        .unwrap();
    assert_eq!(react["label"][0]["value"], json!("^19.0.0"));
    assert_eq!(
        react["label"][0]["tooltip"],
        json!("catalog `default` · package.json")
    );
    let react_dom = hints
        .iter()
        .find(|h| h["position"] == end_of(&text, "\"catalog: legacy \"", 0))
        .unwrap();
    assert_eq!(react_dom["label"][0]["value"], json!("^18.3.1"));
    assert_eq!(
        react_dom["label"][0]["tooltip"],
        json!("catalog `legacy` · package.json")
    );
}

#[test]
fn completion_offers_only_catalogs_that_define_the_package() {
    let mut server = Server::start(Options::default());
    let (uri, text) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let edited = text.replace(
        "\"react-dom\": \"catalog:react18\"",
        "\"react-dom\": \"cat\"",
    );
    server.change(&uri, 2, &edited);
    server.wait_diagnostics(&uri);
    let completion = server.at("textDocument/completion", &uri, end_of(&edited, "\"cat", 1));
    let items = completion.as_array().unwrap();
    let labels: Vec<&str> = items
        .iter()
        .map(|item| item["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        vec!["catalog:", "catalog:react18", "catalog:react19"]
    );
    assert_eq!(items[2]["detail"], json!("^19.0.0"));
    assert_eq!(items[2]["documentation"], json!("pnpm-workspace.yaml"));
    assert_eq!(items[2]["textEdit"]["newText"], json!("catalog:react19"));
    assert_eq!(items[2]["textEdit"]["range"], range_of(&edited, "cat", 1));

    let types_only_in_default = server.at(
        "textDocument/completion",
        &uri,
        end_of(&edited, "\"catalog:react18\"", 0),
    );
    let labels: Vec<&str> = types_only_in_default
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        Vec::<&str>::new(),
        "the cursor after the closing quote is outside the value"
    );
    let inside = server.at(
        "textDocument/completion",
        &uri,
        position_of(&edited, "react18\"", 0),
    );
    let labels: Vec<&str> = inside
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        vec!["catalog:"],
        "@types/react is defined only in the default catalog"
    );

    let no_prefix = server.at("textDocument/completion", &uri, end_of(&edited, "\"app", 0));
    assert!(no_prefix.as_array().unwrap().is_empty());
}

#[test]
fn document_colors_distinguish_catalogs() {
    let mut server = Server::start(Options {
        settings: json!({ "colorAlpha": 0.4 }),
        ..Options::default()
    });
    let (uri, text) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let colors = server.request(
        "textDocument/documentColor",
        json!({ "textDocument": { "uri": uri } }),
    );
    let colors = colors.as_array().unwrap();
    let color_at = |needle: &str, nth: usize| {
        colors
            .iter()
            .find(|c| c["range"] == range_of(&text, needle, nth))
            .map(|c| c["color"].clone())
    };
    let default = color_at("\"catalog:\"", 0).expect("react is coloured");
    let react18 = color_at("\"catalog:react18\"", 0).expect("react-dom is coloured");
    let react19 = color_at("\"catalog:react19\"", 0).expect("override is coloured");
    assert_ne!(default, react18);
    assert_ne!(react18, react19);
    assert!((default["alpha"].as_f64().unwrap() - 0.4).abs() < 1e-6);
    assert_eq!(
        color_at("\"catalog:\"", 2),
        Some(default.clone()),
        "same catalog, same colour"
    );
    assert!(
        color_at("\"catalog:react17\"", 0).is_none(),
        "an undefined catalog has no colour"
    );
    let presentations = server.request(
        "textDocument/colorPresentation",
        json!({ "textDocument": { "uri": uri }, "color": default, "range": range_of(&text, "\"catalog:\"", 0) }),
    );
    assert_eq!(presentations, json!([]));
}

#[test]
fn go_to_catalog_uses_show_document_when_the_client_supports_it() {
    let mut server = Server::start(Options {
        show_document: true,
        ..Options::default()
    });
    assert_eq!(
        server.initialize_result["capabilities"]["executeCommandProvider"]["commands"],
        json!(["catalog-lens.goToCatalog"])
    );
    let (uri, text) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let hints = server.inlay_hints(&uri);
    let command = &hints.as_array().unwrap()[0]["label"][0]["command"];
    assert_eq!(command["command"], json!("catalog-lens.goToCatalog"));
    let lenses = server.code_lenses(&uri);
    let lens = &lenses.as_array().unwrap()[0];
    assert_eq!(
        lens["command"]["command"],
        json!("catalog-lens.goToCatalog")
    );
    let location = lens["command"]["arguments"][0].clone();
    assert_eq!(
        location["uri"],
        json!(uri_for(&fixtures().join("pnpm/pnpm-workspace.yaml")))
    );

    let result = server.request(
        "workspace/executeCommand",
        json!({ "command": "catalog-lens.goToCatalog", "arguments": [location] }),
    );
    assert_eq!(result, json!(true));
    let shown = server.wait_recorded("window/showDocument");
    assert_eq!(shown["params"]["uri"], location["uri"]);
    assert_eq!(shown["params"]["selection"], location["range"]);
    assert_eq!(shown["params"]["takeFocus"], json!(true));
    let _ = text;
}

#[test]
fn without_show_document_the_command_is_not_registered() {
    let server = Server::start(Options::default());
    assert_eq!(
        server.initialize_result["capabilities"]["executeCommandProvider"],
        Value::Null
    );
}

#[test]
fn turning_zed_code_lens_off_brings_versions_back_without_an_edit() {
    let mut server = Server::start(Options {
        code_lens: CodeLens::On,
        ..Options::default()
    });
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    assert!(!server.code_lenses(&uri).as_array().unwrap().is_empty());
    let shown = server.inlay_hints(&uri);
    let shown = server.hints_after_refreshes(&uri, shown);
    assert_eq!(shown, json!([]), "lenses carry the versions");

    server.set_code_lens(CodeLens::Off);
    let shown = server.hints_after_refreshes(&uri, shown);
    assert!(
        !shown.as_array().unwrap().is_empty(),
        "versions are back on the right: {shown}"
    );
}

#[test]
fn turning_zed_code_lens_on_moves_versions_into_lenses() {
    let mut server = Server::start(Options::default());
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let shown = server.inlay_hints(&uri);
    let shown = server.hints_after_refreshes(&uri, shown);
    assert!(!shown.as_array().unwrap().is_empty());

    server.set_code_lens(CodeLens::On);
    let shown = server.hints_after_refreshes(&uri, shown);
    assert_eq!(shown, json!([]), "versions moved into lenses");
    assert!(!server.code_lenses(&uri).as_array().unwrap().is_empty());
}

#[test]
fn zed_code_lens_menu_never_hides_versions() {
    let mut server = Server::start(Options {
        code_lens: CodeLens::Menu,
        ..Options::default()
    });
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let mut shown = server.inlay_hints(&uri);
    for round in 0..3 {
        if round == 1 {
            server.change_settings(json!({}));
        }
        assert!(
            !server.code_lenses(&uri).as_array().unwrap().is_empty(),
            "the menu lists lenses"
        );
        shown = server.hints_after_refreshes(&uri, shown);
        assert!(
            !shown.as_array().unwrap().is_empty(),
            "round {round}: a menu lens request hid the versions"
        );
    }
}

#[test]
fn zed_code_lens_on_keeps_versions_in_lenses_without_probing_again() {
    let mut server = Server::start(Options {
        code_lens: CodeLens::On,
        ..Options::default()
    });
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    server.code_lenses(&uri);
    let shown = server.inlay_hints(&uri);
    assert_eq!(server.hints_after_refreshes(&uri, shown), json!([]));

    server.clear_recorded();
    for _ in 0..3 {
        thread::sleep(Duration::from_millis(800));
        assert_eq!(server.inlay_hints(&uri), json!([]), "scrolled");
    }
    assert_eq!(
        server.recorded("workspace/codeLens/refresh").len(),
        0,
        "lenses already answered a probe"
    );
    assert!(server.recorded("workspace/inlayHint/refresh").is_empty());
}

#[test]
fn no_request_waits_for_a_probe_once_the_mode_is_known() {
    for code_lens in [CodeLens::Off, CodeLens::On, CodeLens::Menu] {
        let mut server = Server::start(Options {
            code_lens,
            ..Options::default()
        });
        let (uri, _) = server.open(&pnpm_app());
        server.wait_diagnostics(&uri);
        if code_lens != CodeLens::Off {
            server.code_lenses(&uri);
        }
        let shown = server.inlay_hints(&uri);
        server.hints_after_refreshes(&uri, shown);

        for round in 0..3 {
            thread::sleep(Duration::from_millis(800));
            let started = Instant::now();
            server.inlay_hints(&uri);
            assert!(
                started.elapsed() < Duration::from_millis(200),
                "{code_lens:?} round {round}: requests took {:?}",
                started.elapsed()
            );
        }
    }
}

#[test]
fn display_inlay_never_answers_lenses_and_display_both_serves_both() {
    let mut server = Server::start(Options {
        settings: json!({ "display": "inlay" }),
        ..Options::default()
    });
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    assert_eq!(server.code_lenses(&uri), json!([]));
    assert!(!server.inlay_hints(&uri).as_array().unwrap().is_empty());
    assert!(server.recorded("workspace/inlayHint/refresh").is_empty());

    let mut server = Server::start(Options {
        settings: json!({ "display": "both" }),
        ..Options::default()
    });
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    assert!(!server.code_lenses(&uri).as_array().unwrap().is_empty());
    assert!(!server.inlay_hints(&uri).as_array().unwrap().is_empty());
}

#[test]
fn feature_switches_turn_hover_diagnostics_colors_and_completion_off() {
    let mut server = Server::start(Options {
        settings: json!({ "hover": false, "diagnostics": false, "documentColors": false, "completion": false }),
        ..Options::default()
    });
    let (uri, text) = server.open(&pnpm_app());
    assert!(server.wait_diagnostics(&uri).is_empty());
    assert_eq!(
        server.at(
            "textDocument/hover",
            &uri,
            position_of(&text, "catalog:react18", 0)
        ),
        Value::Null
    );
    assert_eq!(
        server.request(
            "textDocument/documentColor",
            json!({ "textDocument": { "uri": uri } })
        ),
        json!([])
    );
    assert_eq!(
        server.at(
            "textDocument/completion",
            &uri,
            end_of(&text, "\"catalog:", 0)
        ),
        Value::Null
    );
}

#[test]
fn a_settings_change_relabels_hints_without_a_restart() {
    let mut server = Server::start(Options::default());
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    let before = hints_by_label(&server.inlay_hints(&uri));
    assert!(before.contains_key("^18.3.1"), "{:?}", before.keys());
    assert!(!before.contains_key("react18 → ^18.3.1"));

    server.clear_recorded();
    server.change_settings(json!({ "namedCatalogLabel": true }));
    server.wait_recorded("workspace/inlayHint/refresh");
    let after = hints_by_label(&server.inlay_hints(&uri));
    assert!(
        after.contains_key("react18 → ^18.3.1"),
        "{:?}",
        after.keys()
    );
}

#[test]
fn turning_diagnostics_off_clears_them_and_turning_them_on_restores_them() {
    let mut server = Server::start(Options::default());
    let (uri, _) = server.open(&pnpm_app());
    let original = messages(&server.wait_diagnostics(&uri));
    assert!(!original.is_empty());

    server.change_settings(json!({ "diagnostics": false }));
    assert_eq!(server.wait_diagnostics(&uri), Vec::<Value>::new());

    server.change_settings(json!({}));
    assert_eq!(
        messages(&server.wait_diagnostics(&uri)),
        original,
        "removing the key restores the default"
    );
}

#[test]
fn switching_display_from_inlay_to_code_lens_moves_versions_live() {
    let mut server = Server::start(Options {
        settings: json!({ "display": "inlay" }),
        ..Options::default()
    });
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);
    assert_eq!(server.code_lenses(&uri), json!([]));
    assert!(!server.inlay_hints(&uri).as_array().unwrap().is_empty());

    server.clear_recorded();
    server.change_settings(json!({ "display": "codeLens" }));
    server.wait_recorded("workspace/codeLens/refresh");
    server.wait_recorded("workspace/inlayHint/refresh");
    assert!(!server.code_lenses(&uri).as_array().unwrap().is_empty());
    assert_eq!(server.inlay_hints(&uri), json!([]));
}

#[test]
fn a_settings_notification_that_changes_nothing_refreshes_nothing() {
    let mut server = Server::start(Options {
        settings: json!({ "display": "inlay", "namedCatalogLabel": true }),
        ..Options::default()
    });
    let (uri, _) = server.open(&pnpm_app());
    server.wait_diagnostics(&uri);

    server.clear_recorded();
    server.change_settings(json!({ "display": "inlay", "namedCatalogLabel": true, "surprise": 1 }));
    server.no_notification_within(
        "textDocument/publishDiagnostics",
        Duration::from_millis(300),
    );
    assert!(server.recorded("workspace/inlayHint/refresh").is_empty());
    assert!(server.recorded("workspace/codeLens/refresh").is_empty());
}

#[test]
fn documents_that_are_not_package_json_are_ignored() {
    let mut server = Server::start(Options::default());
    let (uri, _) = server.open(&fixtures().join("pnpm/.zed/settings.json"));
    server.no_notification_within(
        "textDocument/publishDiagnostics",
        Duration::from_millis(300),
    );
    assert_eq!(server.inlay_hints(&uri), Value::Null);
    server.open_text(
        "untitled:1",
        "{ \"dependencies\": { \"react\": \"catalog:\" } }",
    );
    server.no_notification_within(
        "textDocument/publishDiagnostics",
        Duration::from_millis(300),
    );
}

#[test]
fn positions_are_utf16_even_with_emoji_before_the_value() {
    let mut server = Server::start(Options::default());
    let uri = uri_for(&fixtures().join("pnpm/packages/inline/package.json"));
    let text = "{ \"description\": \"🔭 렌즈\", \"dependencies\": { \"react\": \"catalog:\" } }";
    server.open_text(&uri, text);
    assert!(server.wait_diagnostics(&uri).is_empty());
    let hints = server.inlay_hints(&uri);
    assert_eq!(hints[0]["position"], end_of(text, "\"catalog:\"", 0));
    assert_eq!(
        hints[0]["position"]["character"],
        json!(text.encode_utf16().count() as u64 - 4)
    );
}

const BOTH_DEFAULTS_PNPM: &str =
    "`catalog` and `catalogs.default` are both defined in pnpm-workspace.yaml; pnpm rejects this";

#[test]
fn pnpm_reads_catalog_default_as_the_default_catalog_and_trims_names() {
    let temp = TempWorkspace::with_files(&[(
        "pnpm-workspace.yaml",
        "catalog:\n  react: ^18.2.0\n  react-dom: ^18.2.1\ncatalogs:\n  r18:\n    vue: ^3.4.0\n",
    )]);
    let doc = r#"{
  "dependencies": {
    "react": "catalog:default",
    "vue": "catalog: r18 "
  },
  "devDependencies": {
    "react-dom": "catalog: "
  }
}
"#;
    let seen = seen(&temp, doc);
    assert_eq!(
        seen.hints,
        hints(&[
            ("react", "^18.2.0"),
            ("vue", "^3.4.0"),
            ("react-dom", "^18.2.1")
        ])
    );
    assert_eq!(seen.warnings, warnings(&[]));
}

#[test]
fn pnpm_warns_once_when_both_default_spellings_are_declared_and_keeps_resolving() {
    let temp = TempWorkspace::with_files(&[(
        "pnpm-workspace.yaml",
        "catalog:\n  react: ^18.2.0\ncatalogs:\n  default:\n    vue: ^3.4.0\n",
    )]);
    let doc = r#"{
  "dependencies": {
    "react": "catalog:",
    "vue": "catalog:default"
  }
}
"#;
    let seen = seen(&temp, doc);
    assert_eq!(
        seen.hints,
        hints(&[("react", "^18.2.0"), ("vue", "^3.4.0")])
    );
    assert_eq!(seen.warnings, warnings(&[("react", BOTH_DEFAULTS_PNPM)]));
}

#[test]
fn yarn_keeps_catalog_and_catalogs_default_apart() {
    let yarnrc = "catalog:\n  react: ^18.3.1\n  vue: ^3.4.0\ncatalogs:\n  default:\n    react: ^17.0.2\n    lodash: ^4.17.21\n    \"@types/react\": ^17.0.0\n";
    let temp = TempWorkspace::with_files(&[(".yarnrc.yml", yarnrc), ("package.json", "{}\n")]);
    let doc = r#"{
  "dependencies": {
    "react": "catalog:",
    "lodash": "catalog:",
    "vue": "catalog:default",
    "@types/react": "catalog:default"
  }
}
"#;
    let seen = seen(&temp, doc);
    assert_eq!(
        seen.hints,
        hints(&[("react", "^18.3.1"), ("@types/react", "^17.0.0")])
    );
    assert_eq!(
        seen.warnings,
        warnings(&[
            (
                "lodash",
                "\"lodash\" is not defined in the default catalog (.yarnrc.yml)"
            ),
            (
                "vue",
                "\"vue\" is not defined in catalog \"default\" (.yarnrc.yml)"
            ),
        ])
    );
}

#[test]
fn bun_reaches_the_default_catalog_under_every_spelling() {
    let root = r#"{
  "workspaces": {
    "packages": ["app"],
    "catalog": { "react": "^19.0.0", "react-dom": "^19.0.0" },
    "catalogs": {
      "default": { "vue": "^3.4.0", "react-dom": "^18.0.0" },
      "": { "lodash": "^4.17.21" }
    }
  }
}
"#;
    let temp = TempWorkspace::with_files(&[("package.json", root)]);
    let doc = r#"{
  "dependencies": {
    "react": "catalog:default",
    "vue": "catalog:",
    "lodash": "catalog:",
    "react-dom": "catalog:"
  }
}
"#;
    let seen = seen(&temp, doc);
    assert_eq!(
        seen.hints,
        hints(&[
            ("react", "^19.0.0"),
            ("vue", "^3.4.0"),
            ("lodash", "^4.17.21"),
            ("react-dom", "^19.0.0"),
        ])
    );
    assert_eq!(
        seen.warnings,
        warnings(&[(
            "react-dom",
            "\"react-dom\" is defined in both `catalog` and `catalogs.default` in package.json; package managers reject this"
        )])
    );
}

#[test]
fn an_empty_named_catalog_is_declared_but_lacks_the_package() {
    let temp = TempWorkspace::with_files(&[(
        "pnpm-workspace.yaml",
        "catalogs:\n  empty: {}\n  r18:\n    react: ^18.2.0\n",
    )]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:empty\"\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc).warnings,
        warnings(&[(
            "react",
            "\"react\" is not defined in catalog \"empty\" (pnpm-workspace.yaml)"
        )])
    );
}

#[test]
fn bun_keeps_the_first_of_duplicate_catalog_entries() {
    let root = r#"{
  "workspaces": {
    "packages": ["app"],
    "catalog": { "react": "^19.0.0" },
    "catalogs": {
      "t": { "jest": "29.0.0", "jest": "30.0.0" },
      "": { "react": "^18.0.0" }
    }
  }
}
"#;
    let temp = TempWorkspace::with_files(&[("package.json", root)]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\",\n    \"jest\": \"catalog:t\"\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc).hints,
        hints(&[("react", "^19.0.0"), ("jest", "29.0.0")])
    );
}

#[test]
fn bun_reads_workspaces_catalogs_before_top_level_ones() {
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\",\n    \"jest\": \"catalog:t\"\n  }\n}\n";
    let workspaces_win = TempWorkspace::with_files(&[(
        "package.json",
        r#"{"catalog": {"react": "^18.0.0"}, "workspaces": {"packages": ["app"], "catalog": {"react": "^19.0.0"}, "catalogs": {"t": {"jest": "30.0.0"}}}}"#,
    )]);
    assert_eq!(
        seen(&workspaces_win, doc),
        Seen {
            hints: hints(&[("react", "^19.0.0"), ("jest", "30.0.0")]),
            warnings: warnings(&[]),
        }
    );

    let top_level_ignored = TempWorkspace::with_files(&[(
        "package.json",
        r#"{"catalog": {"react": "^18.0.0"}, "workspaces": {"packages": ["app"], "catalogs": {"t": {"jest": "30.0.0"}}}}"#,
    )]);
    assert_eq!(
        seen(&top_level_ignored, doc),
        Seen {
            hints: hints(&[("jest", "30.0.0")]),
            warnings: warnings(&[(
                "react",
                "\"react\" is not defined in the default catalog (package.json)"
            )]),
        }
    );

    let workspaces_array = TempWorkspace::with_files(&[(
        "package.json",
        r#"{"workspaces": ["app"], "catalog": {"react": "^19.0.0"}, "catalogs": {"t": {"jest": "30.0.0"}}}"#,
    )]);
    assert_eq!(
        seen(&workspaces_array, doc).hints,
        hints(&[("react", "^19.0.0"), ("jest", "30.0.0")])
    );
}

#[test]
fn bun_ignores_top_level_catalogs_without_a_workspaces_field() {
    let temp = TempWorkspace::with_files(&[(
        "package.json",
        r#"{"name": "root", "catalog": {"react": "^19.0.0"}}"#,
    )]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc).warnings,
        warnings(&[(
            "react",
            "No catalog definitions found: expected pnpm-workspace.yaml, .yarnrc.yml, or a package.json with a `catalog` field in a parent directory"
        )])
    );
}

#[test]
fn yaml_merge_keys_lose_to_explicit_keys_in_any_order() {
    let temp = TempWorkspace::with_files(&[(
        "pnpm-workspace.yaml",
        "a: &a\n  react: ^17.0.0\n  vue: ^3.4.0\n  jest: 29.0.0\nb: &b\n  jest: 30.0.0\n  lodash: ^4.17.21\ncatalog:\n  react: ^18.2.0\n  <<: [*a, *b]\n",
    )]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\",\n    \"vue\": \"catalog:\",\n    \"jest\": \"catalog:\",\n    \"lodash\": \"catalog:\"\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc),
        Seen {
            hints: hints(&[
                ("react", "^18.2.0"),
                ("vue", "^3.4.0"),
                ("jest", "29.0.0"),
                ("lodash", "^4.17.21"),
            ]),
            warnings: warnings(&[]),
        }
    );
}

const SCALARS_YAML: &str =
    "catalog:\n  a: 18\n  b: 1.0\n  c: true\n  d: 0x1F\n  e: ~\n  f:\n  g: null\n  h: '18'\n";
const SCALARS_DOC: &str = "{\n  \"dependencies\": {\n    \"a\": \"catalog:\",\n    \"b\": \"catalog:\",\n    \"c\": \"catalog:\",\n    \"d\": \"catalog:\",\n    \"e\": \"catalog:\",\n    \"f\": \"catalog:\",\n    \"g\": \"catalog:\",\n    \"h\": \"catalog:\"\n  }\n}\n";

fn not_in_default(packages: &str, file: &str) -> Vec<(String, String)> {
    packages
        .chars()
        .map(|p| {
            (
                p.to_string(),
                format!("\"{p}\" is not defined in the default catalog ({file})"),
            )
        })
        .collect()
}

#[test]
fn pnpm_takes_only_string_yaml_values_as_versions() {
    let temp = TempWorkspace::with_files(&[("pnpm-workspace.yaml", SCALARS_YAML)]);
    assert_eq!(
        seen(&temp, SCALARS_DOC),
        Seen {
            hints: hints(&[("h", "18")]),
            warnings: not_in_default("abcdefg", "pnpm-workspace.yaml"),
        }
    );
}

#[test]
fn yarn_reads_yaml_scalars_as_strings_but_not_nulls() {
    let temp =
        TempWorkspace::with_files(&[(".yarnrc.yml", SCALARS_YAML), ("package.json", "{}\n")]);
    assert_eq!(
        seen(&temp, SCALARS_DOC),
        Seen {
            hints: hints(&[
                ("a", "18"),
                ("b", "1.0"),
                ("c", "true"),
                ("d", "0x1F"),
                ("h", "18")
            ]),
            warnings: not_in_default("efg", ".yarnrc.yml"),
        }
    );
}

#[test]
fn yaml_block_scalars_label_without_newline_and_span_every_line() {
    let yaml = "catalog:\n  a: |\n    ^1.0.0\n  b: >\n    ^2.0.0\n    || ^3.0.0\n\n  c: ^4.0.0\n";
    let temp = TempWorkspace::with_files(&[("pnpm-workspace.yaml", yaml)]);
    let doc = "{\n  \"dependencies\": {\n    \"a\": \"catalog:\",\n    \"b\": \"catalog:\",\n    \"c\": \"catalog:\"\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc).hints,
        hints(&[("a", "^1.0.0"), ("b", "^2.0.0 || ^3.0.0"), ("c", "^4.0.0")])
    );
    let mut server = Server::start(Options::default());
    let uri = uri_for(&temp.path("app/package.json"));
    server.open_text(&uri, doc);
    server.wait_diagnostics(&uri);
    let definition = server.at(
        "textDocument/definition",
        &uri,
        position_of(doc, "\"catalog:\"", 1),
    );
    assert_eq!(
        definition["range"],
        json!({ "start": position_of(yaml, "^2.0.0", 0), "end": end_of(yaml, "|| ^3.0.0", 0) })
    );
}

#[test]
fn a_leading_byte_order_mark_is_not_part_of_the_document() {
    let doc = "\u{feff}{\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    let yaml = "\u{feff}catalog: {react: ^18.2.0}\n";
    let pnpm = TempWorkspace::with_files(&[("pnpm-workspace.yaml", yaml)]);
    assert_eq!(
        seen(&pnpm, doc),
        Seen {
            hints: hints(&[("react", "^18.2.0")]),
            warnings: warnings(&[]),
        }
    );
    let mut server = Server::start(Options::default());
    let uri = uri_for(&pnpm.path("app/package.json"));
    server.open_text(&uri, doc);
    server.wait_diagnostics(&uri);
    let hint = &server.inlay_hints(&uri)[0];
    assert_eq!(hint["position"], end_of(doc, "\"catalog:\"", 0));
    assert_eq!(
        hint["label"][0]["location"]["range"],
        range_of(yaml, "^18.2.0", 0),
        "the mark counts as one UTF-16 unit, as in the editor"
    );

    let bun = TempWorkspace::with_files(&[(
        "package.json",
        "\u{feff}{\"workspaces\": {\"packages\": [\"app\"], \"catalog\": {\"react\": \"^19.0.0\"}}}\n",
    )]);
    assert_eq!(seen(&bun, doc).hints, hints(&[("react", "^19.0.0")]));
}

#[test]
fn an_escaped_non_ascii_character_does_not_swallow_the_rest_of_the_document() {
    let temp =
        TempWorkspace::with_files(&[("pnpm-workspace.yaml", "catalog:\n  react: ^18.2.0\n")]);
    let doc = "{\n  \"description\": \"\\é\",\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    assert_eq!(seen(&temp, doc).hints, hints(&[("react", "^18.2.0")]));
}

#[test]
fn deeply_nested_json_neither_crashes_nor_hides_references() {
    let depth = 100_000;
    let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    let temp = TempWorkspace::with_files(&[(
        "package.json",
        &format!(
            "{{\"workspaces\": {{\"packages\": [\"app\"], \"catalog\": {{\"react\": \"^19.0.0\"}}}}, \"x\": {nested}}}\n"
        ),
    )]);
    let doc = format!(
        "{{\n  \"dependencies\": {{\n    \"react\": \"catalog:\"\n  }},\n  \"x\": {nested}\n}}\n"
    );
    assert_eq!(seen(&temp, &doc).hints, hints(&[("react", "^19.0.0")]));
}

fn time_to_resolve(temp: &TempWorkspace) -> (Duration, BTreeMap<String, String>) {
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    let started = Instant::now();
    let seen = seen(temp, doc);
    (started.elapsed(), seen.hints)
}

#[test]
fn large_catalog_files_resolve_in_linear_time() {
    const ENTRIES: usize = 20_000;
    const BUDGET: Duration = Duration::from_secs(3);
    let yaml: String = std::iter::once("catalog:\n".to_owned())
        .chain((0..ENTRIES).map(|i| format!("  pkg-{i:06}: ^1.{i}.0\n")))
        .chain(["  react: ^18.2.0\n".to_owned()])
        .collect();
    let (elapsed, hints_seen) = time_to_resolve(&TempWorkspace::with_files(&[(
        "pnpm-workspace.yaml",
        &yaml,
    )]));
    assert_eq!(hints_seen, hints(&[("react", "^18.2.0")]));
    assert!(elapsed < BUDGET, "{ENTRIES}-entry YAML took {elapsed:?}");

    let entries: Vec<String> = (0..ENTRIES)
        .map(|i| format!("\"pkg-{i:06}\": \"^1.{i}.0\""))
        .collect();
    let bun = format!(
        "{{\"workspaces\": {{\"packages\": [\"app\"], \"catalog\": {{{}, \"react\": \"^18.2.0\"}}}}}}",
        entries.join(", ")
    );
    let (elapsed, hints_seen) =
        time_to_resolve(&TempWorkspace::with_files(&[("package.json", &bun)]));
    assert_eq!(hints_seen, hints(&[("react", "^18.2.0")]));
    assert!(
        elapsed < BUDGET,
        "one-line {ENTRIES}-entry bun root took {elapsed:?}"
    );
}

#[test]
fn a_dot_key_in_npm_overrides_refers_to_the_enclosing_package() {
    let temp = TempWorkspace::with_files(&[(
        "pnpm-workspace.yaml",
        "catalog:\n  foo: ^1.0.0\n  bar: ^2.0.0\n",
    )]);
    let doc = "{\n  \"overrides\": {\n    \"foo@1\": {\n      \".\": \"catalog:\",\n      \"bar\": \"catalog:\"\n    }\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc),
        Seen {
            hints: hints(&[(".", "^1.0.0"), ("bar", "^2.0.0")]),
            warnings: warnings(&[]),
        }
    );
}

#[test]
fn opening_and_closing_an_edited_bun_root_republishes_its_dependents() {
    let temp = TempWorkspace::copy_of("bun");
    let mut server = Server::start(Options::default());
    let (app_uri, _) = server.open(&temp.path("packages/app/package.json"));
    assert_eq!(
        messages(&server.wait_diagnostics(&app_uri)),
        Vec::<String>::new()
    );

    let root = temp.path("package.json");
    let root_uri = uri_for(&root);
    let unsaved = std::fs::read_to_string(&root)
        .unwrap()
        .replace(",\n        \"react-dom\": \"^18.3.1\"", "");
    assert!(!unsaved.contains("\"react-dom\": \"^18.3.1\""));
    server.open_text(&root_uri, &unsaved);
    assert_eq!(
        messages(&server.wait_diagnostics(&app_uri)),
        vec!["\"react-dom\" is not defined in catalog \"legacy\" (package.json)"],
        "the unsaved root buffer wins once it is open"
    );

    server.notify(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": root_uri } }),
    );
    assert_eq!(
        messages(&server.wait_diagnostics(&app_uri)),
        Vec::<String>::new(),
        "closing the buffer falls back to the file on disk"
    );
}

#[test]
fn an_unchanged_workspace_file_is_not_read_again() {
    const REQUESTS: u32 = 40;
    let yaml = format!("# {}\ncatalog:\n  react: ^18.2.0\n", "x".repeat(32 << 20));
    let temp = TempWorkspace::with_files(&[("pnpm-workspace.yaml", &yaml)]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    let mut server = Server::start(Options::default());
    let uri = uri_for(&temp.path("app/package.json"));
    temp.write("app/package.json", doc);
    server.open_text(&uri, doc);
    server.wait_diagnostics(&uri);
    let started = Instant::now();
    for _ in 0..REQUESTS {
        let hover = server.at("textDocument/hover", &uri, position_of(doc, "catalog:", 0));
        assert_ne!(hover, Value::Null);
    }
    let per_request = started.elapsed() / REQUESTS;
    assert!(
        per_request < Duration::from_millis(2),
        "each hover took {per_request:?} next to a 32 MiB workspace file"
    );
}

#[test]
fn a_yaml_file_with_lone_carriage_returns_still_resolves() {
    let temp = TempWorkspace::with_files(&[(
        "pnpm-workspace.yaml",
        "catalog:\r  vue: ^3.4.0\r  react: ^18.2.0\r",
    )]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    assert_eq!(seen(&temp, doc).hints, hints(&[("react", "^18.2.0")]));
}

fn offset_at(text: &str, position: &Value) -> usize {
    let line = position["line"].as_u64().unwrap() as usize;
    let line_start: usize = text.split_inclusive('\n').take(line).map(str::len).sum();
    let mut units = position["character"].as_u64().unwrap() as usize;
    let mut offset = line_start;
    for ch in text[line_start..].chars() {
        if units == 0 {
            break;
        }
        units -= ch.len_utf16();
        offset += ch.len_utf8();
    }
    offset
}

fn definition_texts(
    temp: &TempWorkspace,
    definition_file: &str,
    doc: &str,
) -> BTreeMap<String, String> {
    let mut server = Server::start(Options::default());
    let uri = uri_for(&temp.path("app/package.json"));
    temp.write("app/package.json", doc);
    server.open_text(&uri, doc);
    server.wait_diagnostics(&uri);
    let definitions = std::fs::read_to_string(temp.path(definition_file)).unwrap();
    doc.lines()
        .enumerate()
        .filter_map(|(line, text)| Some((line, text.find("\"catalog:")?)))
        .map(|(line, character)| {
            let position = json!({ "line": line, "character": character });
            let range = &server.at("textDocument/definition", &uri, position)["range"];
            let selected = if range.is_null() {
                "<no definition>".to_owned()
            } else {
                definitions[offset_at(&definitions, &range["start"])
                    ..offset_at(&definitions, &range["end"])]
                    .to_owned()
            };
            (key_on_line(doc, line as u64), selected)
        })
        .collect()
}

#[test]
fn yaml_definitions_select_only_the_value_after_anchors_tags_and_flow_neighbours() {
    let yaml = "catalog:\n  react: &r ^18.2.0\n  react-dom: *r\n  typescript: !!str 18\n  jest: &j !!str 29\n  lodash: !!str &l \"^4.17.21\"   # pinned\n  vitest: &v\n    ^1.6.0\ncatalogs:\n  flow: &f {vue: ^3.4.0, pinia: ^2.1.0}\n  tagged: !!map {zod: ^3.23.0}\n";
    let temp = TempWorkspace::with_files(&[("pnpm-workspace.yaml", yaml)]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\",\n    \"react-dom\": \"catalog:\",\n    \"typescript\": \"catalog:\",\n    \"jest\": \"catalog:\",\n    \"lodash\": \"catalog:\",\n    \"vitest\": \"catalog:\",\n    \"vue\": \"catalog:flow\",\n    \"pinia\": \"catalog:flow\",\n    \"zod\": \"catalog:tagged\"\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc),
        Seen {
            hints: hints(&[
                ("react", "^18.2.0"),
                ("react-dom", "^18.2.0"),
                ("typescript", "18"),
                ("jest", "29"),
                ("lodash", "^4.17.21"),
                ("vitest", "^1.6.0"),
                ("vue", "^3.4.0"),
                ("pinia", "^2.1.0"),
                ("zod", "^3.23.0"),
            ]),
            warnings: warnings(&[]),
        }
    );
    assert_eq!(
        definition_texts(&temp, "pnpm-workspace.yaml", doc),
        hints(&[
            ("react", "^18.2.0"),
            ("react-dom", "*r"),
            ("typescript", "18"),
            ("jest", "29"),
            ("lodash", "\"^4.17.21\""),
            ("vitest", "^1.6.0"),
            ("vue", "^3.4.0"),
            ("pinia", "^2.1.0"),
            ("zod", "^3.23.0"),
        ])
    );
}

fn nested_aliases(levels: usize) -> String {
    let mut yaml = "a0: &a0 [lol]\n".to_owned();
    for level in 1..=levels {
        let aliases = vec![format!("*a{}", level - 1); 10].join(", ");
        yaml.push_str(&format!("a{level}: &a{level} [{aliases}]\n"));
    }
    yaml + "catalog:\n  react: ^18.2.0\n"
}

#[test]
fn a_yaml_file_with_nested_aliases_still_resolves() {
    let temp = TempWorkspace::with_files(&[("pnpm-workspace.yaml", &nested_aliases(4))]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc),
        Seen {
            hints: hints(&[("react", "^18.2.0")]),
            warnings: warnings(&[]),
        }
    );
}

#[test]
fn an_alias_bomb_is_refused_instead_of_expanded() {
    let temp = TempWorkspace::with_files(&[("pnpm-workspace.yaml", &nested_aliases(6))]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc).warnings,
        warnings(&[(
            "react",
            "pnpm-workspace.yaml does not parse; no catalogs available"
        )])
    );
}

#[test]
fn pnpm_rejects_a_file_with_a_duplicate_key() {
    let temp = TempWorkspace::with_files(&[(
        "pnpm-workspace.yaml",
        "catalogs:\n  r:\n    react: ^17.0.0\n    react: ^18.2.0\n",
    )]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:r\"\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc),
        Seen {
            hints: hints(&[]),
            warnings: warnings(&[(
                "react",
                "pnpm-workspace.yaml does not parse; no catalogs available"
            )]),
        }
    );
}

#[test]
fn yarn_keeps_the_last_of_duplicate_catalog_entries() {
    let temp = TempWorkspace::with_files(&[
        (
            ".yarnrc.yml",
            "catalog:\n  react: ^17.0.2\n  react: ^18.3.1\n",
        ),
        ("package.json", "{}\n"),
    ]);
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    assert_eq!(
        seen(&temp, doc),
        Seen {
            hints: hints(&[("react", "^18.3.1")]),
            warnings: warnings(&[]),
        }
    );
}

#[test]
fn the_package_manager_a_directory_names_owns_its_catalogs() {
    let bun_root = |package_manager: &str| {
        format!(
            r#"{{{package_manager}"workspaces": {{"packages": ["app"], "catalog": {{"react": "^19.0.0"}}}}}}"#
        )
    };
    let pnpm = ("pnpm-workspace.yaml", "catalog:\n  react: ^18.2.0\n");
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    let resolved = |files: &[(&str, &str)]| seen(&TempWorkspace::with_files(files), doc).hints;

    let named = bun_root(r#""packageManager": "bun@1.3.0", "#);
    assert_eq!(
        resolved(&[("package.json", &named), pnpm]),
        hints(&[("react", "^19.0.0")]),
        "packageManager names Bun"
    );
    let unnamed = bun_root("");
    assert_eq!(
        resolved(&[("package.json", &unnamed), ("bun.lock", "{}\n"), pnpm]),
        hints(&[("react", "^19.0.0")]),
        "bun.lock names Bun"
    );
    assert_eq!(
        resolved(&[
            ("package.json", "{}\n"),
            ("yarn.lock", ""),
            (".yarnrc.yml", "catalog:\n  react: ^17.0.2\n"),
            pnpm,
        ]),
        hints(&[("react", "^17.0.2")]),
        "yarn.lock names Yarn"
    );
    let pnpm_named = bun_root(r#""packageManager": "pnpm@9.15.0+sha512.abc", "#);
    assert_eq!(
        resolved(&[("package.json", &pnpm_named), ("bun.lock", "{}\n"), pnpm]),
        hints(&[("react", "^18.2.0")]),
        "packageManager outranks a lockfile"
    );
    let bun_without_catalog = r#"{"packageManager": "bun@1.3.0", "workspaces": ["app"]}"#;
    assert_eq!(
        resolved(&[("package.json", bun_without_catalog), pnpm]),
        hints(&[("react", "^18.2.0")]),
        "a named manager without definitions falls back to the others"
    );
}

#[test]
fn an_empty_version_resolves_in_pnpm_but_not_in_yarn() {
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\",\n    \"vue\": \"catalog:\",\n    \"lodash\": \"catalog:e\"\n  }\n}\n";
    let yaml = "catalog:\n  react: ''\n  vue: ^3.4.0\ncatalogs:\n  e:\n    lodash: \"\"\n";
    let pnpm = TempWorkspace::with_files(&[("pnpm-workspace.yaml", yaml)]);
    assert_eq!(
        seen(&pnpm, doc),
        Seen {
            hints: hints(&[("react", "\"\""), ("vue", "^3.4.0"), ("lodash", "\"\"")]),
            warnings: warnings(&[]),
        }
    );
    let yarn = TempWorkspace::with_files(&[(".yarnrc.yml", yaml), ("package.json", "{}\n")]);
    assert_eq!(
        seen(&yarn, doc),
        Seen {
            hints: hints(&[("vue", "^3.4.0")]),
            warnings: warnings(&[
                (
                    "react",
                    "\"react\" is not defined in the default catalog (.yarnrc.yml)"
                ),
                (
                    "lodash",
                    "\"lodash\" is not defined in catalog \"e\" (.yarnrc.yml)"
                ),
            ]),
        }
    );
}

#[test]
fn pnpm_rejects_catalog_entries_that_use_catalog_link_or_file() {
    let yaml = "catalog:\n  c: link:./packages/c\n  d: file:./t/d.tgz\n  e: catalog:other\n  f: ^1.0.0\n  g: catalog\ncatalogs:\n  n:\n    h: link:../h\n";
    let doc = "{\n  \"dependencies\": {\n    \"c\": \"catalog:\",\n    \"d\": \"catalog:\",\n    \"e\": \"catalog:\",\n    \"f\": \"catalog:\",\n    \"g\": \"catalog:\",\n    \"h\": \"catalog:n\"\n  }\n}\n";
    let pnpm = TempWorkspace::with_files(&[("pnpm-workspace.yaml", yaml)]);
    assert_eq!(
        seen(&pnpm, doc),
        Seen {
            hints: hints(&[("f", "^1.0.0"), ("g", "catalog")]),
            warnings: warnings(&[
                (
                    "c",
                    "\"c\" in the default catalog uses the link: protocol (pnpm-workspace.yaml); pnpm rejects this"
                ),
                (
                    "d",
                    "\"d\" in the default catalog uses the file: protocol (pnpm-workspace.yaml); pnpm rejects this"
                ),
                (
                    "e",
                    "\"e\" in the default catalog uses the catalog: protocol (pnpm-workspace.yaml); pnpm rejects this"
                ),
                (
                    "h",
                    "\"h\" in catalog \"n\" uses the link: protocol (pnpm-workspace.yaml); pnpm rejects this"
                ),
            ]),
        }
    );
    let yarn = TempWorkspace::with_files(&[(".yarnrc.yml", yaml), ("package.json", "{}\n")]);
    assert_eq!(
        seen(&yarn, doc).hints,
        hints(&[
            ("c", "link:./packages/c"),
            ("d", "file:./t/d.tgz"),
            ("e", "catalog:other"),
            ("f", "^1.0.0"),
            ("g", "catalog"),
            ("h", "link:../h"),
        ]),
        "Yarn accepts them"
    );
}

#[test]
fn a_package_under_node_modules_gets_no_versions_or_warnings() {
    let doc = "{\n  \"dependencies\": {\n    \"react\": \"catalog:\"\n  }\n}\n";
    let temp = TempWorkspace::with_files(&[
        ("pnpm-workspace.yaml", "catalog:\n  react: ^18.2.0\n"),
        ("app/package.json", doc),
        ("node_modules/foo/package.json", doc),
        (
            "node_modules/.pnpm/foo@1.0.0/node_modules/foo/package.json",
            doc,
        ),
    ]);
    let outside = TempWorkspace::with_files(&[("node_modules/bar/package.json", doc)]);
    let mut server = Server::start(Options::default());
    let (app, _) = server.open(&temp.path("app/package.json"));
    server.wait_diagnostics(&app);
    assert_eq!(server.inlay_hints(&app).as_array().unwrap().len(), 1);
    for path in [
        temp.path("node_modules/foo/package.json"),
        temp.path("node_modules/.pnpm/foo@1.0.0/node_modules/foo/package.json"),
        outside.path("node_modules/bar/package.json"),
    ] {
        let (uri, text) = server.open(&path);
        assert_eq!(
            server.wait_diagnostics(&uri),
            Vec::<Value>::new(),
            "{path:?}"
        );
        assert_eq!(server.inlay_hints(&uri), json!([]), "{path:?}");
        assert_eq!(
            server.at(
                "textDocument/hover",
                &uri,
                position_of(&text, "catalog:", 0)
            ),
            Value::Null
        );
    }
}
