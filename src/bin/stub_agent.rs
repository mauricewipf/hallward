//! Scripted stand-in for an agent CLI, so `tests/ask_ai.rs` can drive a whole
//! Ask AI request with no network, no credentials, and no live agent.
//!
//! The scenario is a JSON file named by `STUB_AGENT_SCENARIO`:
//!
//! ```json
//! {
//!   "argv_log": "/tmp/x/argv.log",
//!   "ask":  { "stdout": "A red car." },
//!   "edit": { "write_png": true, "stdout": "photo-edited.png" }
//! }
//! ```
//!
//! Which step applies is decided from the prompt, because one Ask AI request
//! can call the same executable twice. Release packaging copies only
//! `target/release/hallward`, so this binary never ships.

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

/// Opening words of `photo_edit_prompt`, absent from `photo_qa_prompt`.
const EDIT_MARKER: &str = "You are a photo editor";
/// `photo_edit_prompt` names the file it wants written right after this.
const DEST_MARKER: &str = "named exactly ";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(scenario) = load_scenario() else {
        eprintln!("stub_agent: STUB_AGENT_SCENARIO must name a readable JSON scenario");
        return ExitCode::from(2);
    };
    let call = if args.iter().any(|arg| arg.contains(EDIT_MARKER)) {
        "edit"
    } else {
        "ask"
    };
    append_argv_log(&scenario, call, &args);
    let step = &scenario[call];

    let sleep_ms = step["sleep_ms"].as_u64().unwrap_or(0);
    if sleep_ms > 0 {
        thread::sleep(Duration::from_millis(sleep_ms));
    }
    if step["write_png"].as_bool().unwrap_or(false) {
        let Some(dest) = step["write_name"]
            .as_str()
            .map(str::to_string)
            .or_else(|| dest_name(&args))
        else {
            eprintln!("stub_agent: no \"{DEST_MARKER}<file>\" in the prompt");
            return ExitCode::from(3);
        };
        if let Err(error) = write_png(Path::new(&dest)) {
            eprintln!("stub_agent: could not write {dest}: {error}");
            return ExitCode::from(3);
        }
    }
    if let Some(text) = step["stdout"].as_str() {
        println!("{text}");
    }
    if let Some(text) = step["stderr"].as_str() {
        eprintln!("{text}");
    }
    ExitCode::from(step["exit"].as_u64().unwrap_or(0) as u8)
}

fn load_scenario() -> Option<Value> {
    let path = env::var_os("STUB_AGENT_SCENARIO")?;
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Append one JSON line per call so tests can assert on the real argv and on
/// the tool-permission environment the agent was handed.
fn append_argv_log(scenario: &Value, call: &str, args: &[String]) {
    let Some(path) = scenario["argv_log"].as_str() else {
        return;
    };
    let record = json!({
        "call": call,
        "argv": args,
        "cwd": env::current_dir().unwrap_or_default(),
        "opencode_permission": env::var("OPENCODE_PERMISSION").ok(),
    });
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{record}");
    }
}

fn dest_name(args: &[String]) -> Option<String> {
    let tail = args.iter().find_map(|arg| arg.split_once(DEST_MARKER))?.1;
    let name = tail.split_whitespace().next()?;
    Some(name.to_string())
}

fn write_png(dest: &Path) -> Result<(), image::ImageError> {
    image::RgbImage::from_pixel(24, 16, image::Rgb([90, 140, 200])).save(dest)
}
