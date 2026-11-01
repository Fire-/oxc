//! Loads JS/TS config files in the pure Rust build, by running them in a `node` child process.
//!
//! `node_config.mjs` mirrors how the Node.js build loads them in-process,
//! so both builds accept and reject the same files.
//! Every load is a fresh process, so an edited config is never served from Node.js's module cache.

use std::{
    ffi::OsStr,
    io::{self, Read},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::Value;

const HELPER: &str = include_str!("node_config.mjs");
/// Starts the result line, so whatever the config prints can't be mistaken for it.
const RESULT_MARKER: &str = "\u{1e}oxfmt-node-config:";
/// A cold load of a large `vite.config.ts` through `vite-plus` takes about 0.1s.
const TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Set to `1` to never run `node`: JS/TS configs then fail to load, and Vite+ mode is not detected.
const OPT_OUT_ENV: &str = "OXFMT_NO_NODE_CONFIG";

#[derive(Debug, Clone, Copy)]
pub enum ConfigKind {
    /// `oxfmt.config.ts` and the like: the default export is the config.
    Oxfmt,
    /// `vite.config.*` in Vite+ mode: the `fmt` field of the config that `vite-plus` resolves.
    VitePlus,
}

#[derive(Debug)]
pub enum LoadError {
    /// The config itself failed: it threw, or its export has the wrong shape.
    Config(String),
    /// `node` could not load the config: missing, crashed or timed out.
    Node(String),
}

pub fn is_disabled() -> bool {
    std::env::var_os(OPT_OUT_ENV).is_some_and(|value| value == "1")
}

/// Load the config at `path`, an absolute path.
///
/// Returns `Ok(None)` for a Vite+ config without a `fmt` field.
pub fn load(path: &Path, kind: ConfigKind) -> Result<Option<Value>, LoadError> {
    if is_disabled() {
        return Err(LoadError::Node(format!(
            "Loading JS/TS config files is turned off by `{OPT_OUT_ENV}=1`."
        )));
    }
    blocking(|| run(OsStr::new("node"), path, kind, TIMEOUT))
}

/// Waiting for `node` blocks the thread, so a Tokio worker hands its other tasks off first.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

#[derive(Deserialize)]
struct NodeResult {
    ok: bool,
    #[serde(default)]
    value: Value,
    #[serde(default)]
    error: String,
}

fn run(
    node: &OsStr,
    path: &Path,
    kind: ConfigKind,
    timeout: Duration,
) -> Result<Option<Value>, LoadError> {
    let mode = match kind {
        ConfigKind::Oxfmt => "oxfmt",
        ConfigKind::VitePlus => "vite",
    };
    let mut command = Command::new(node);
    command
        .args(["--input-type=module", "--eval", HELPER, "--", mode])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = path.parent() {
        command.current_dir(dir);
    }
    if matches!(kind, ConfigKind::VitePlus) {
        // `vp fmt` sets this for oxfmt, and `vite-plus` skips work it doesn't need for the config
        command.env("VP_RESOLVING_CONFIG_METADATA", "1");
    }

    let mut child = command.spawn().map_err(|err| {
        LoadError::Node(if err.kind() == io::ErrorKind::NotFound {
            "`node` was not found on PATH. Node.js >=22.18.0 is needed to load JS/TS config files."
                .to_string()
        } else {
            format!("Failed to run `node`: {err}")
        })
    })?;
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(LoadError::Node(format!(
                    "`node` did not finish loading the config within {timeout:?}."
                )));
            }
            Err(err) => return Err(LoadError::Node(format!("Failed to wait for `node`: {err}"))),
        }
    };

    // Processes the config started may still hold the pipes open
    let grace = deadline.saturating_duration_since(Instant::now()).max(Duration::from_secs(1));
    let stdout = stdout.recv_timeout(grace).unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout);
    let Some(line) = stdout.lines().rev().find_map(|line| line.strip_prefix(RESULT_MARKER)) else {
        let stderr = stderr.recv_timeout(grace).unwrap_or_default();
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(LoadError::Node(format!(
            "`node` exited ({status}) without loading the config.\n{}",
            stderr.trim_end()
        )));
    };

    let result: NodeResult = serde_json::from_str(line)
        .map_err(|err| LoadError::Node(format!("Unreadable result from `node`: {err}")))?;
    if result.ok {
        Ok((!result.value.is_null()).then_some(result.value))
    } else {
        Err(LoadError::Config(result.error))
    }
}

fn read_in_background(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    }
    rx
}

#[cfg(test)]
mod test {
    use std::{fs, path::Path, time::Duration};

    use serde_json::json;

    use super::{ConfigKind, LoadError, OsStr, TIMEOUT, run};

    fn load(path: &Path, kind: ConfigKind) -> Result<Option<serde_json::Value>, LoadError> {
        run(OsStr::new("node"), path, kind, TIMEOUT)
    }

    fn config_error(result: Result<Option<serde_json::Value>, LoadError>) -> String {
        match result {
            Err(LoadError::Config(err)) => err,
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    fn node_error(result: Result<Option<serde_json::Value>, LoadError>) -> String {
        match result {
            Err(LoadError::Node(err)) => err,
            other => panic!("expected a node error, got {other:?}"),
        }
    }

    #[test]
    fn loads_the_default_export() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxfmt.config.ts");
        fs::write(
            &path,
            "const config: Record<string, unknown> = { semi: false, overrides: [] };\n\
             export default config;\n",
        )
        .unwrap();
        let value = load(&path, ConfigKind::Oxfmt).unwrap();
        assert_eq!(value, Some(json!({ "semi": false, "overrides": [] })));
    }

    #[test]
    fn output_from_the_config_is_not_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxfmt.config.mjs");
        fs::write(
            &path,
            "console.log('{\"semi\":true}');\n\
             process.stdout.write('\\u001eoxfmt-node-config:{\"ok\":true,\"value\":{}}\\n');\n\
             export default { semi: false };\n",
        )
        .unwrap();
        assert_eq!(load(&path, ConfigKind::Oxfmt).unwrap(), Some(json!({ "semi": false })));
    }

    #[test]
    fn rejects_configs_like_the_nodejs_build() {
        let dir = tempfile::tempdir().unwrap();
        for (source, expected) in [
            ("throw new Error(\"config exploded\");", "config exploded"),
            ("export const semi = false;", "Configuration file has no default export."),
            ("export default [];", "must have a default export that is an object"),
            ("export default {", "Unexpected end of input"),
        ] {
            let path = dir.path().join("oxfmt.config.mjs");
            fs::write(&path, source).unwrap();
            let err = config_error(load(&path, ConfigKind::Oxfmt));
            assert!(err.contains(expected), "`{source}`: {err}");
        }
    }

    #[test]
    fn a_hanging_config_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxfmt.config.mjs");
        fs::write(&path, "setInterval(() => {}, 1000);\nawait new Promise(() => {});\n").unwrap();
        let result = run(OsStr::new("node"), &path, ConfigKind::Oxfmt, Duration::from_millis(500));
        assert!(node_error(result).contains("did not finish"));
    }

    #[test]
    fn missing_node_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxfmt.config.mjs");
        fs::write(&path, "export default {};").unwrap();
        let missing = dir.path().join("no-such-node");
        let result = run(missing.as_os_str(), &path, ConfigKind::Oxfmt, TIMEOUT);
        assert!(node_error(result).contains("`node` was not found"));
    }

    #[test]
    fn a_crashing_node_is_reported_with_its_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxfmt.config.mjs");
        fs::write(&path, "console.error('last words');\nprocess.exit(3);\n").unwrap();
        let err = node_error(load(&path, ConfigKind::Oxfmt));
        assert!(err.contains("without loading the config") && err.contains("last words"), "{err}");
    }

    #[test]
    fn vite_plus_config_gives_its_fmt_field() {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("node_modules").join("vite-plus");
        fs::create_dir_all(&stub).unwrap();
        fs::write(stub.join("package.json"), r#"{ "type": "module", "exports": "./index.js" }"#)
            .unwrap();
        // Also checks that `vp fmt`'s environment is passed on
        fs::write(
            stub.join("index.js"),
            "import { pathToFileURL } from \"node:url\";\n\
             export async function resolveConfig({ configFile }, command) {\n\
             \x20 const config = (await import(pathToFileURL(configFile).href)).default;\n\
             \x20 if (!config.echo) return config;\n\
             \x20 return { fmt: { command, metadata: process.env.VP_RESOLVING_CONFIG_METADATA } };\n\
             }\n",
        )
        .unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let path = sub.join("vite.config.mjs");

        fs::write(&path, "export default { fmt: { semi: false }, lint: {} };").unwrap();
        assert_eq!(load(&path, ConfigKind::VitePlus).unwrap(), Some(json!({ "semi": false })));

        fs::write(&path, "export default {};").unwrap();
        assert_eq!(load(&path, ConfigKind::VitePlus).unwrap(), None);

        fs::write(&path, "export default { fmt: [] };").unwrap();
        let err = config_error(load(&path, ConfigKind::VitePlus));
        assert!(err.contains("The `fmt` field in the default export must be an object."), "{err}");

        fs::write(&path, "export default { echo: true };").unwrap();
        let value = load(&path, ConfigKind::VitePlus).unwrap();
        assert_eq!(value, Some(json!({ "command": "build", "metadata": "1" })));
    }

    #[test]
    fn vite_plus_config_needs_vite_plus_installed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vite.config.mjs");
        fs::write(&path, "export default { fmt: {} };").unwrap();
        let err = config_error(load(&path, ConfigKind::VitePlus));
        assert!(err.contains("vite-plus"), "{err}");
    }
}
