//! Drives the pure Rust `oxfmt --lsp` over stdio, as an editor extension does.
#![cfg(not(feature = "napi"))]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use oxc_language_server::uri_utils::file_path_to_uri;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

struct Server {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
}

impl Server {
    fn start(cwd: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_oxfmt"))
            .arg("--lsp")
            .current_dir(cwd)
            // oxc-vscode sets this; the logs must not end up in the protocol stream on stdout
            .env("OXC_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn `oxfmt --lsp`");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (tx, messages) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(message) = read_message(&mut reader) {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });

        Self { child, stdin, messages }
    }

    fn send(&mut self, message: &Value) {
        let body = message.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Option<Value>) {
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(&message);
    }

    fn request(&mut self, id: u64, method: &str, params: Option<Value>) -> Value {
        let mut message = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(&message);

        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = self
                .messages
                .recv_timeout(remaining)
                .unwrap_or_else(|err| panic!("no response to `{method}`: {err}"));
            // Skip notifications and server-to-client requests
            if message["id"] == id && message.get("method").is_none() {
                return message;
            }
        }
    }

    fn wait(mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("`oxfmt --lsp` did not exit after `exit`");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Read one `Content-Length` framed message, `None` at end of stream.
fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = Some(value.parse::<usize>().expect("invalid Content-Length"));
        }
    }
    let mut body = vec![0; length.expect("message without Content-Length")];
    reader.read_exact(&mut body).ok()?;
    Some(serde_json::from_slice(&body).expect("message body is not JSON"))
}

fn open_and_format(
    server: &mut Server,
    id: u64,
    uri: &str,
    language_id: &str,
    text: &str,
) -> Value {
    server.notify(
        "textDocument/didOpen",
        Some(json!({
            "textDocument": { "uri": uri, "languageId": language_id, "version": 1, "text": text }
        })),
    );
    server.request(
        id,
        "textDocument/formatting",
        Some(json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": 2, "insertSpaces": true }
        })),
    )
}

#[test]
fn formats_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join(".oxfmtrc.json"), r#"{ "semi": false }"#).unwrap();
    // Same content on disk, in case `didOpen` is handled after the formatting request
    fs::write(root.join("a.ts"), "const a = 1;\n").unwrap();
    fs::write(root.join("a.vue"), "<template><div   ></div></template>\n").unwrap();
    let uri = |path: &Path| file_path_to_uri(path).unwrap().to_string();
    let root_uri = uri(root);

    let mut server = Server::start(root);

    let initialize = server.request(
        1,
        "initialize",
        Some(json!({
            "processId": null,
            "capabilities": {},
            "rootUri": null,
            "workspaceFolders": [{ "uri": root_uri, "name": "test" }],
            // What oxc-vscode sends
            "initializationOptions": [{
                "workspaceUri": root_uri,
                "options": { "fmt.experimental": true, "fmt.configPath": null }
            }]
        })),
    );
    assert_eq!(initialize["result"]["serverInfo"]["name"], "oxfmt", "{initialize}");
    assert_eq!(initialize["result"]["capabilities"]["documentFormattingProvider"], true);
    server.notify("initialized", Some(json!({})));

    let ts =
        open_and_format(&mut server, 2, &uri(&root.join("a.ts")), "typescript", "const a = 1;\n");
    assert_eq!(
        ts["result"],
        json!([{
            "range": {
                "start": { "line": 0, "character": 11 },
                "end": { "line": 0, "character": 12 }
            },
            "newText": ""
        }]),
        "{ts}"
    );

    // Prettier-backed languages need the Node.js build: no edits, no error
    let vue = open_and_format(
        &mut server,
        3,
        &uri(&root.join("a.vue")),
        "vue",
        "<template><div   ></div></template>\n",
    );
    assert!(vue.get("error").is_none(), "{vue}");
    assert!(vue["result"].as_array().is_none_or(Vec::is_empty), "{vue}");

    let shutdown = server.request(4, "shutdown", None);
    assert!(shutdown.get("error").is_none(), "{shutdown}");
    server.notify("exit", None);
    assert!(server.wait().success());
}
