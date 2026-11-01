// Loads a config file for the pure Rust oxfmt, run as
// `node --input-type=module --eval <this file> -- <oxfmt|vite> <absolute path>` from the config's directory.
//
// Mirrors how the Node.js build loads configs in-process
// (`importJsConfig` and `loadViteConfigField` in `apps/shared/src-js`),
// so both builds accept and reject the same files.
//
// Writes one line to stdout: the marker, then `{"ok":true,"value":...}` or `{"ok":false,"error":"..."}`.

import { extname } from "node:path";
import { pathToFileURL } from "node:url";

const RESULT_MARKER = "\u001eoxfmt-node-config:";

// Anything the config prints goes to stderr, so stdout carries only the result
const writeResult = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);

const isObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);

function typeScriptHint(err, path) {
  if (![".ts", ".mts", ".cts"].includes(extname(path).toLowerCase())) return null;
  const message = err instanceof Error ? err.message : String(err);
  const unknownExtension =
    err?.code === "ERR_UNKNOWN_FILE_EXTENSION" ||
    /unknown(?: or unsupported)? file extension/i.test(message);
  if (!unknownExtension) return null;
  return `${message}\n\nTypeScript config files require Node.js >=22.18.0.\nDetected Node.js ${process.version}.\nPlease upgrade Node.js or use a JSON config file instead.`;
}

async function loadOxfmtConfig(path) {
  let module;
  try {
    module = await import(pathToFileURL(path).href);
  } catch (err) {
    const hint = typeScriptHint(err, path);
    if (hint) throw new Error(hint, { cause: err });
    throw err;
  }
  if (module.default === undefined) {
    throw new Error("Configuration file has no default export.");
  }
  if (!isObject(module.default)) {
    throw new Error("Configuration file must have a default export that is an object.");
  }
  return module.default;
}

// `null` when there is no `fmt` field, which means "no config here"
async function loadVitePlusConfig(path) {
  // Resolved from the config's directory, the project's own install
  const { resolveConfig } = await import("vite-plus");
  const config = await resolveConfig({ configFile: path }, "build");
  if (!("fmt" in config)) return null;
  if (!isObject(config.fmt)) {
    throw new Error("The `fmt` field in the default export must be an object.");
  }
  return config.fmt;
}

const [mode, path] = process.argv.slice(1);
let result;
try {
  const value = mode === "vite" ? await loadVitePlusConfig(path) : await loadOxfmtConfig(path);
  result = JSON.stringify({ ok: true, value });
} catch (err) {
  result = JSON.stringify({ ok: false, error: err instanceof Error ? err.message : String(err) });
}
// Exit right away: a config may leave timers or handles open
writeResult(`${RESULT_MARKER}${result}\n`, () => process.exit(0));
