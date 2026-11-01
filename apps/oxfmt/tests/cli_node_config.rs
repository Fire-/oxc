//! Runs the pure Rust `oxfmt` CLI on projects whose config needs Node.js to load.
#![cfg(not(feature = "napi"))]

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn oxfmt(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oxfmt"))
        .args(args)
        .current_dir(cwd)
        .env_remove("VP_VERSION")
        .env_remove("OXFMT_NO_NODE_CONFIG")
        .output()
        .expect("failed to run `oxfmt`")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

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

#[test]
fn check_uses_root_and_nested_js_configs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("oxfmt.config.ts"), "export default { singleQuote: true };\n").unwrap();
    fs::write(root.join("a.ts"), "const a = 'x';\n").unwrap();
    let sub = root.join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("oxfmt.config.ts"), "export default { semi: false };\n").unwrap();
    fs::write(sub.join("b.ts"), "const b = \"x\";\n").unwrap();
    fs::write(sub.join("c.ts"), "const c = \"x\"\n").unwrap();

    let output = oxfmt(root, &["--check"]);
    assert_eq!(output.status.code(), Some(1), "{}\n{}", stdout(&output), stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("sub/b.ts"), "{out}");
    assert!(!out.contains("a.ts") && !out.contains("c.ts"), "{out}");
}

#[test]
fn vite_config_fmt_field_is_used_when_vite_plus_is_installed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    install_stub_vite_plus(root);
    fs::write(root.join("vite.config.ts"), "export default { fmt: { semi: false } };\n").unwrap();
    // Ignored in Vite+ mode, as with `vp fmt`
    fs::write(root.join(".oxfmtrc.json"), r#"{ "singleQuote": true }"#).unwrap();
    let sub = root.join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("vite.config.ts"), "export default { fmt: { singleQuote: true } };\n")
        .unwrap();
    fs::write(root.join("a.ts"), "const a = \"x\";\n").unwrap();
    fs::write(sub.join("b.ts"), "const b = \"x\";\n").unwrap();

    let output = oxfmt(root, &["a.ts", "sub/b.ts"]);
    assert_eq!(output.status.code(), Some(0), "{}\n{}", stdout(&output), stderr(&output));
    assert_eq!(fs::read_to_string(root.join("a.ts")).unwrap(), "const a = \"x\"\n");
    assert_eq!(fs::read_to_string(sub.join("b.ts")).unwrap(), "const b = \"x\"\n");
}

#[test]
fn js_config_that_fails_to_load_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("oxfmt.config.ts"), "throw new Error(\"config exploded\");\n").unwrap();
    fs::write(root.join("a.ts"), "const a=1\n").unwrap();

    let output = oxfmt(root, &[]);
    assert_eq!(output.status.code(), Some(1), "{}\n{}", stdout(&output), stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("Failed to load configuration file."), "{err}");
    assert!(err.contains("config exploded"), "{err}");
    assert_eq!(fs::read_to_string(root.join("a.ts")).unwrap(), "const a=1\n");
}
