//! Headless Ask AI: resolve a vision-capable agent and run an image prompt.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::catalog::Photo;
use crate::image_edit::{self, SavedEdit};
use crate::media::{bin_on_path, is_image};
use crate::thumbs;

pub const ASK_TIMEOUT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(50);
const ERROR_CAP: usize = 800;
const DOT_STEP: Duration = Duration::from_millis(400);

const SUPPORTED: &[&str] = &["opencode", "pi", "omp", "hermes", "codex", "claude"];
const STUB_MAX_BYTES: usize = 16 * 1024;

/// Progress shown while a headless Ask AI / edit job is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskProgress {
    Analyzing,
    Editing,
    Indexing,
}

/// Successful Ask AI outcome: a text answer, or a newly saved edited still.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskValue {
    Answer(String),
    Saved(SavedEdit),
}

/// Result of one headless Ask AI run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOutcome {
    pub id: u64,
    pub result: Result<AskValue, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskDecision {
    Answer(String),
    Edit(String),
}

/// An agent name paired with the executable that implements it. Tests point
/// `program` at a stub binary so a whole Ask AI run needs no live agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCli {
    pub agent: String,
    pub program: PathBuf,
}

impl AgentCli {
    /// Run the agent by name, resolved on PATH, the way the TUI does.
    pub fn on_path(agent: &str) -> Self {
        Self::with_program(agent, agent)
    }

    /// Build the agent's argv but run it through `program`.
    pub fn with_program(agent: &str, program: impl Into<PathBuf>) -> Self {
        Self {
            agent: agent.to_string(),
            program: program.into(),
        }
    }
}

/// Wall-clock budgets for the two agent calls one Ask AI request can make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    pub ask: Duration,
    pub edit: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            ask: ASK_TIMEOUT,
            edit: image_edit::EDIT_TIMEOUT,
        }
    }
}

/// Handle for an in-flight Ask AI child process.
pub struct AskHandle {
    pub id: u64,
    rx: Receiver<AskOutcome>,
    cancel: Arc<AtomicBool>,
    child_slot: Arc<Mutex<Option<Child>>>,
    progress: Arc<Mutex<AskProgress>>,
}

impl AskHandle {
    pub fn try_recv(&mut self) -> Option<AskOutcome> {
        match self.rx.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(AskOutcome {
                id: self.id,
                result: Err("The AI request ended unexpectedly.".into()),
            }),
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        kill_slot(&self.child_slot);
    }

    pub fn progress(&self) -> AskProgress {
        *self
            .progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for AskHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// True on Linux when `/etc/os-release` is Omarchy or an Omarchy CLI is on PATH.
pub fn is_omarchy() -> bool {
    is_omarchy_from(
        cfg!(target_os = "linux"),
        std::fs::read_to_string("/etc/os-release").ok().as_deref(),
        bin_on_path("omarchy") || bin_on_path("omarchy-default-agent"),
    )
}

pub fn is_omarchy_from(linux: bool, os_release: Option<&str>, bin_present: bool) -> bool {
    if !linux {
        return false;
    }
    os_release.is_some_and(os_release_is_omarchy) || bin_present
}

pub fn os_release_is_omarchy(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        match line.split_once('=') {
            Some(("ID", value)) => unwrap_release_value(value) == "omarchy",
            _ => false,
        }
    })
}

fn unwrap_release_value(value: &str) -> &str {
    let v = value.trim();
    if (v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')) {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

/// Inputs for agent resolution (kept injectable so tests never touch PATH).
#[derive(Debug, Clone, Default)]
pub struct ResolveInput {
    pub allow_path: bool,
    pub omarchy_default: Option<String>,
    pub real_on_path: Vec<String>,
}

/// Pick the Omarchy default when set, else the first real supported CLI on PATH.
pub fn resolve_agent_from(input: &ResolveInput) -> Option<String> {
    if let Some(name) = input
        .omarchy_default
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(canonicalize_agent)
    {
        return Some(name);
    }
    if !input.allow_path {
        return None;
    }
    SUPPORTED
        .iter()
        .find(|name| input.real_on_path.iter().any(|n| n == *name))
        .map(|s| (*s).to_string())
}

pub fn ask_platform_ok(macos: bool, omarchy: bool) -> bool {
    macos || omarchy
}

/// Resolve the agent Hallward should call. Cached by the TUI; refresh on send.
pub fn resolve_agent() -> Option<String> {
    let omarchy = is_omarchy();
    resolve_agent_from(&ResolveInput {
        allow_path: ask_platform_ok(cfg!(target_os = "macos"), omarchy),
        omarchy_default: read_omarchy_default(),
        real_on_path: SUPPORTED
            .iter()
            .copied()
            .filter(|name| is_real_agent(name))
            .map(str::to_string)
            .collect(),
    })
}

fn which_bin(bin: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(bin))
            .find(|p| p.is_file())
    })
}

fn omarchy_reports_missing(name: &str) -> bool {
    if !bin_on_path("omarchy-cmd-missing") {
        return false;
    }
    Command::new("omarchy-cmd-missing")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .is_some_and(|s| s.success())
}

pub fn looks_like_omarchy_stub(bytes: &[u8]) -> bool {
    if bytes.len() > STUB_MAX_BYTES || bytes.starts_with(&[0x7f, b'E', b'L', b'F']) {
        return false;
    }
    if bytes.len() >= 4 {
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if matches!(
            magic,
            0xfeed_face | 0xfeed_facf | 0xcafe_babe | 0xcffa_edfe | 0xcefa_edfe
        ) {
            return false;
        }
    }
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    text.contains("mise use")
        || text.contains("mise exec")
        || text.contains("mise x ")
        || text.contains("omarchy-mise")
        || text.contains("omarchy-install")
        || text.contains("omarchy-cmd-missing")
}

fn is_real_agent(name: &str) -> bool {
    if omarchy_reports_missing(name) {
        return false;
    }
    let Some(path) = which_bin(name) else {
        return false;
    };
    fs::read(&path)
        .ok()
        .is_some_and(|bytes| !looks_like_omarchy_stub(&bytes))
}

/// Read the Omarchy default agent from its config file, then `omarchy-default-agent`.
pub fn read_omarchy_default() -> Option<String> {
    if let Ok(contents) = fs::read_to_string(agent_file_path()) {
        if let Some(name) = parse_agent_file(&contents) {
            return Some(name);
        }
    }
    let output = Command::new("omarchy-default-agent")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_agent_file(&String::from_utf8_lossy(&output.stdout))
}

/// Canonical agent name, or None when unset.
pub fn canonicalize_agent(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(
        match raw {
            "pi" => "pi",
            "omp" | "oh-my-pi" => "omp",
            "opencode" | "open-code" => "opencode",
            "ori" | "openrouter" => "ori",
            "claude" | "claude-code" => "claude",
            "codex" => "codex",
            "crush" => "crush",
            "grok" => "grok",
            "agy" | "antigravity" | "antigravity-cli" | "gemini" | "gemini-cli" => "agy",
            "hermes" => "hermes",
            "copilot" | "github-copilot" => "copilot",
            other => other,
        }
        .to_string(),
    )
}

pub fn parse_agent_file(contents: &str) -> Option<String> {
    canonicalize_agent(contents.lines().next().unwrap_or(""))
}

pub fn agent_file_path() -> PathBuf {
    dirs_home()
        .map(|h| h.join(".config/omarchy/defaults/agent"))
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
}

fn dirs_home() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

pub fn marked_still_rels(photos: &[Photo], marked: &HashSet<String>) -> Vec<String> {
    photos
        .iter()
        .filter(|p| marked.contains(&p.relpath) && is_image(Path::new(&p.relpath)))
        .map(|p| p.relpath.clone())
        .collect()
}

pub fn abs_stills(root: &Path, rels: &[String]) -> Vec<PathBuf> {
    rels.iter().map(|r| root.join(r)).collect()
}

pub fn ask_ai_active(agent: Option<&str>, stills: &[String]) -> bool {
    agent.is_some() && !stills.is_empty()
}

pub fn waiting_text(phase: AskProgress, started: Instant, now: Instant) -> String {
    let base = match phase {
        AskProgress::Analyzing => "Analyzing prompt",
        AskProgress::Editing => "Editing image",
        AskProgress::Indexing => "Indexing result",
    };
    let step = now
        .checked_duration_since(started)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        / DOT_STEP.as_millis();
    match step % 4 {
        0 => base.into(),
        1 => format!("{base}."),
        2 => format!("{base}.."),
        _ => format!("{base}..."),
    }
}

pub fn wrap_line_count(text: &str, width: u16) -> u16 {
    let w = width.max(1) as usize;
    let mut lines = 0u16;
    for para in text.split('\n') {
        if para.is_empty() {
            lines = lines.saturating_add(1);
            continue;
        }
        let n = para.chars().count();
        let wrapped = n.div_ceil(w);
        lines = lines.saturating_add(wrapped.max(1) as u16);
    }
    lines.max(1)
}

pub const ASK_PANE_MAX: u16 = 12;

pub fn search_pane_height(ask_ai: bool, prompt: &str, second: Option<&str>, width: u16) -> u16 {
    if !ask_ai {
        return 3;
    }
    let inner_w = width.saturating_sub(2).max(1);
    let prompt_lines = wrap_line_count(prompt, inner_w);
    let extra = match second {
        None => 0,
        Some(s) => 1 + wrap_line_count(s, inner_w),
    };
    (2 + prompt_lines + extra).clamp(3, ASK_PANE_MAX)
}

pub fn is_supported_agent(agent: &str) -> bool {
    SUPPORTED.contains(&agent)
}

pub fn unsupported_message(agent: &str) -> String {
    format!("Ask AI does not yet support {agent}. Use opencode, pi, omp, hermes, codex, or claude.")
}

pub fn no_agent_message() -> String {
    no_agent_message_for(is_omarchy())
}

pub fn no_agent_message_for(omarchy: bool) -> String {
    if omarchy {
        "No vision-capable agent found. Set one with: omarchy default agent — or install opencode, pi, omp, hermes, codex, or claude.".into()
    } else {
        "No vision-capable agent found on PATH (opencode, pi, omp, hermes, codex, claude).".into()
    }
}

pub fn not_installed_message(agent: &str) -> String {
    not_installed_message_for(agent, is_omarchy())
}

pub fn not_installed_message_for(agent: &str, omarchy: bool) -> String {
    let name = display_name(agent);
    if omarchy {
        format!("{name} is not installed. Choose an installed agent with: omarchy default agent")
    } else {
        format!("{name} is not installed. Install it and ensure it is on PATH.")
    }
}

pub fn no_images_message() -> String {
    "Videos can't be sent to the AI. Mark a photo and try again.".into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolPolicy {
    DenyAll,
    AllowWrite,
}

fn photo_qa_prompt(user_prompt: &str) -> String {
    format!(
        "You are a photo assistant. Analyze only the attached images themselves. \
Do not inspect files, databases, metadata, paths, or the working directory. \
Do not use or mention tools. Do not reveal reasoning or narrate your process.\n\n\
If the user wants the photograph itself changed (remove, add, retouch, restyle, \
replace, or otherwise alter pixels), reply with exactly one JSON object and nothing else:\n\
{{\"edit\":\"<imperative instruction for the image editor>\"}}\n\
The edit value must be a complete instruction that can be applied to the attached image, \
preserving everything the user did not mention.\n\n\
Otherwise return only the direct final answer to the user's question.\n\n\
User request:\n{user_prompt}"
    )
}

pub fn parse_ask_decision(text: &str) -> AskDecision {
    if let Some(instruction) = parse_edit_directive(text) {
        AskDecision::Edit(instruction)
    } else {
        AskDecision::Answer(text.trim().to_string())
    }
}

fn parse_edit_directive(text: &str) -> Option<String> {
    let candidate = strip_markdown_fence(text.trim());
    let value: Value = serde_json::from_str(candidate).ok()?;
    let object = value.as_object()?;
    let instruction = object.get("edit")?.as_str()?.trim();
    if instruction.is_empty() {
        return None;
    }
    Some(instruction.to_string())
}

fn strip_markdown_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest
        .strip_prefix("json")
        .or_else(|| rest.strip_prefix("JSON"))
        .unwrap_or(rest);
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .or_else(|| rest.strip_prefix('\r'))
        .unwrap_or(rest);
    match rest.strip_suffix("```") {
        Some(inner) => inner.trim(),
        None => trimmed,
    }
}

fn attachment_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// OpenCode binds the default model to `--dir` (the project). Pointing that at
/// a throwaway preview folder makes it fall back to an undeployed Console
/// model, so `--dir` stays the process cwd and `-f` uses absolute paths.
fn opencode_dir_and_files<P: AsRef<Path>>(files: &[P]) -> Vec<String> {
    let mut argv = Vec::new();
    if let Ok(dir) = env::current_dir() {
        argv.push("--dir".into());
        argv.push(dir.to_string_lossy().into_owned());
    }
    for file in files {
        argv.push("-f".into());
        argv.push(absolute_attach(file.as_ref()));
    }
    argv
}

fn absolute_attach(path: &Path) -> String {
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    env::current_dir()
        .map(|cwd| cwd.join(path).to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// Headless argv for a verified image-capable Omarchy agent.
pub fn build_argv(agent: &str, prompt: &str, files: &[PathBuf]) -> Result<Vec<String>, String> {
    if files.is_empty() {
        return Err(no_images_message());
    }
    if !is_supported_agent(agent) {
        return Err(unsupported_message(agent));
    }
    Ok(match agent {
        "opencode" => {
            let mut argv = vec!["opencode".into(), "run".into(), photo_qa_prompt(prompt)];
            argv.extend(opencode_dir_and_files(files));
            argv
        }
        "pi" => {
            let mut argv = vec![
                "pi".into(),
                "-p".into(),
                "--no-tools".into(),
                "--no-context-files".into(),
                "--no-session".into(),
            ];
            for f in files {
                argv.push(format!("@{}", attachment_name(f)));
            }
            argv.push("--".into());
            argv.push(photo_qa_prompt(prompt));
            argv
        }
        "omp" => {
            let mut argv = vec![
                "omp".into(),
                "-p".into(),
                "--no-tools".into(),
                "--no-session".into(),
            ];
            for f in files {
                argv.push(format!("@{}", attachment_name(f)));
            }
            argv.push("--".into());
            argv.push(photo_qa_prompt(prompt));
            argv
        }
        "hermes" => {
            let mut argv = vec![
                "hermes".into(),
                "chat".into(),
                "--oneshot".into(),
                "-Q".into(),
                "--safe-mode".into(),
            ];
            for f in files {
                argv.push("--image".into());
                argv.push(attachment_name(f));
            }
            argv.push("-q".into());
            argv.push(photo_qa_prompt(prompt));
            argv
        }
        "codex" => {
            let mut argv = vec![
                "codex".into(),
                "exec".into(),
                "--ephemeral".into(),
                "--sandbox".into(),
                "read-only".into(),
                photo_qa_prompt(prompt),
            ];
            for f in files {
                argv.push("-i".into());
                argv.push(attachment_name(f));
            }
            argv
        }
        "claude" => {
            let mut body = String::from("Look at these images:\n");
            for f in files {
                body.push_str(&attachment_name(f));
                body.push('\n');
            }
            body.push('\n');
            body.push_str(&photo_qa_prompt(prompt));
            vec![
                "claude".into(),
                "-p".into(),
                "--tools".into(),
                "Read".into(),
                "--permission-mode".into(),
                "dontAsk".into(),
                "--".into(),
                body,
            ]
        }
        other => return Err(unsupported_message(other)),
    })
}

fn photo_edit_prompt(instruction: &str, source_name: &str, dest_name: &str) -> String {
    format!(
        "You are a photo editor. Edit only the attached photograph according to the instruction. \
Save the edited image as a new file named exactly {dest_name} in the current directory. \
Do not overwrite {source_name} or any other file. \
Do not add text, watermarks, or captions. \
When finished, reply with only the saved filename.\n\n\
Instruction:\n{instruction}"
    )
}

/// Headless argv that asks the agent to write an edited sibling image.
pub fn build_edit_argv(
    agent: &str,
    instruction: &str,
    source: &Path,
    dest: &Path,
) -> Result<Vec<String>, String> {
    if !is_supported_agent(agent) {
        return Err(unsupported_message(agent));
    }
    let source_name = attachment_name(source);
    let dest_name = attachment_name(dest);
    let prompt = photo_edit_prompt(instruction, &source_name, &dest_name);
    Ok(match agent {
        "opencode" => {
            let mut argv = vec!["opencode".into(), "run".into(), prompt];
            argv.extend(opencode_dir_and_files(&[source]));
            argv
        }
        "pi" => vec![
            "pi".into(),
            "-p".into(),
            "--no-context-files".into(),
            "--no-session".into(),
            format!("@{source_name}"),
            "--".into(),
            prompt,
        ],
        "omp" => vec![
            "omp".into(),
            "-p".into(),
            "--no-session".into(),
            format!("@{source_name}"),
            "--".into(),
            prompt,
        ],
        "hermes" => vec![
            "hermes".into(),
            "chat".into(),
            "--oneshot".into(),
            "-Q".into(),
            "--image".into(),
            source_name,
            "-q".into(),
            prompt,
        ],
        "codex" => vec![
            "codex".into(),
            "exec".into(),
            "--ephemeral".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            prompt,
            "-i".into(),
            source_name,
        ],
        "claude" => vec![
            "claude".into(),
            "-p".into(),
            "--tools".into(),
            "Read,Write".into(),
            "--permission-mode".into(),
            "dontAsk".into(),
            "--".into(),
            format!("Look at this image:\n{source_name}\n\n{prompt}"),
        ],
        other => return Err(unsupported_message(other)),
    })
}

pub fn sign_in_hint(agent: &str) -> String {
    match agent {
        "opencode" => format!(
            "{name} is not signed in. Run `{cmd}` and configure an API key.",
            name = display_name(agent),
            cmd = "opencode auth login"
        ),
        "pi" => format!(
            "{name} is not signed in. Open Pi and run `/login`, or set an API key.",
            name = display_name(agent)
        ),
        "omp" => format!(
            "{name} is not signed in. Open Oh My Pi and sign in, or set an API key.",
            name = display_name(agent)
        ),
        "hermes" => format!(
            "{name} is not signed in. Configure credentials for Hermes and try again.",
            name = display_name(agent)
        ),
        "codex" => format!(
            "{name} is not signed in. Run `{cmd}`.",
            name = display_name(agent),
            cmd = "codex login"
        ),
        "claude" => format!(
            "{name} is not signed in. Run `/login` in Claude Code or set ANTHROPIC_API_KEY.",
            name = display_name(agent)
        ),
        other => format!(
            "{name} is not signed in. Configure an API key for your Omarchy default agent.",
            name = display_name(other)
        ),
    }
}

pub fn display_name(agent: &str) -> &str {
    match agent {
        "opencode" => "OpenCode",
        "pi" => "Pi",
        "omp" => "Oh My Pi",
        "hermes" => "Hermes",
        "codex" => "Codex",
        "claude" => "Claude Code",
        "ori" => "Ori",
        "crush" => "Crush",
        "grok" => "Grok",
        "agy" => "Antigravity",
        "copilot" => "GitHub Copilot",
        other => other,
    }
}

pub fn looks_like_auth_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("api key")
        || lower.contains("no auth credentials")
        || lower.contains("not logged in")
        || lower.contains("authentication required")
        || lower.contains("missing authentication")
        || lower.contains("missing api key")
        || lower.contains("no api key found")
}

pub fn looks_like_vision_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("does not support image")
        || lower.contains("doesn't support image")
        || lower.contains("no image content")
        || lower.contains("image input")
        || lower.contains("multimodal")
        || lower.contains("vision-capable")
        || lower.contains("cannot read images")
        || (lower.contains("modalities") && lower.contains("image"))
}

pub fn map_error_text(agent: &str, text: &str) -> String {
    let cleaned = cap_error(&strip_ansi(text));
    if looks_like_auth_error(&cleaned) {
        return sign_in_hint(agent);
    }
    if looks_like_vision_error(&cleaned) {
        return format!(
            "The configured {} model cannot read images. Choose a vision-capable default model.",
            display_name(agent)
        );
    }
    if cleaned.trim().is_empty() {
        format!("{} returned an error.", display_name(agent))
    } else {
        cleaned
    }
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for d in chars.by_ref() {
                        if d.is_ascii_alphabetic() || d == '~' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    for d in chars.by_ref() {
                        if d == '\u{7}' || d == '\u{1b}' {
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if c == '\u{7}' {
            continue;
        }
        out.push(c);
    }
    out
}

fn cap_error(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= ERROR_CAP {
        t.to_string()
    } else {
        let mut s: String = t.chars().take(ERROR_CAP).collect();
        s.push('…');
        s
    }
}

fn kill_slot(slot: &Mutex<Option<Child>>) {
    if let Ok(mut guard) = slot.lock() {
        if let Some(child) = guard.as_mut() {
            let pid = child.id();
            let _ = child.kill();
            kill_process_group(pid);
        }
    }
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let pgid = format!("-{pid}");
    let _ = Command::new("kill")
        .args(["-TERM", &pgid])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Spawn a headless Ask AI request on a background thread.
pub fn spawn(
    id: u64,
    agent: String,
    prompt: String,
    files: Vec<PathBuf>,
    library_root: PathBuf,
) -> AskHandle {
    spawn_with(
        id,
        AgentCli::on_path(&agent),
        prompt,
        files,
        library_root,
        Timeouts::default(),
    )
}

/// [`spawn`] with an explicit executable and timeouts, so tests can drive a
/// full request against a stub agent in milliseconds.
pub fn spawn_with(
    id: u64,
    cli: AgentCli,
    prompt: String,
    files: Vec<PathBuf>,
    library_root: PathBuf,
    timeouts: Timeouts,
) -> AskHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let child_slot = Arc::new(Mutex::new(None));
    let progress = Arc::new(Mutex::new(AskProgress::Analyzing));
    let cancel_t = cancel.clone();
    let slot_t = child_slot.clone();
    let progress_t = progress.clone();
    thread::spawn(move || {
        let result = run_ask(
            &cli,
            &prompt,
            &files,
            &library_root,
            &cancel_t,
            &slot_t,
            &progress_t,
            timeouts,
        );
        let _ = tx.send(AskOutcome { id, result });
    });
    AskHandle {
        id,
        rx,
        cancel,
        child_slot,
        progress,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_ask(
    cli: &AgentCli,
    prompt: &str,
    files: &[PathBuf],
    library_root: &Path,
    cancel: &AtomicBool,
    child_slot: &Mutex<Option<Child>>,
    progress: &Mutex<AskProgress>,
    timeouts: Timeouts,
) -> Result<AskValue, String> {
    if cancel.load(Ordering::SeqCst) {
        return Err("Ask AI cancelled.".into());
    }
    set_progress(progress, AskProgress::Analyzing);
    let (preview_dir, prepared_files) = prepare_agent_files(files)?;
    let argv = build_argv(&cli.agent, prompt, &prepared_files)?;
    let answer = execute_argv(
        cli,
        &argv,
        preview_dir.path(),
        cancel,
        child_slot,
        timeouts.ask,
        ToolPolicy::DenyAll,
    )?;
    match parse_ask_decision(&answer) {
        AskDecision::Answer(text) => {
            if text.is_empty() {
                Err(format!("{} returned no answer.", display_name(&cli.agent)))
            } else {
                Ok(AskValue::Answer(text))
            }
        }
        AskDecision::Edit(instruction) => {
            edit_source_count_ok(files.len())?;
            set_progress(progress, AskProgress::Editing);
            let saved = run_agent_edit(
                cli,
                &files[0],
                library_root,
                &instruction,
                cancel,
                child_slot,
                progress,
                timeouts.edit,
            )?;
            Ok(AskValue::Saved(saved))
        }
    }
}

fn set_progress(progress: &Mutex<AskProgress>, value: AskProgress) {
    if let Ok(mut guard) = progress.lock() {
        *guard = value;
    }
}

pub fn edit_source_count_ok(count: usize) -> Result<(), String> {
    if count == 1 {
        Ok(())
    } else {
        Err(image_edit::edit_needs_one_photo_message())
    }
}

#[allow(clippy::too_many_arguments)]
fn run_agent_edit(
    cli: &AgentCli,
    source: &Path,
    library_root: &Path,
    instruction: &str,
    cancel: &AtomicBool,
    child_slot: &Mutex<Option<Child>>,
    progress: &Mutex<AskProgress>,
    timeout: Duration,
) -> Result<SavedEdit, String> {
    if cancel.load(Ordering::SeqCst) {
        return Err("Ask AI cancelled.".into());
    }
    let dest = image_edit::unique_sibling_path(source, "png")?;
    let work_dir = source.parent().unwrap_or_else(|| Path::new("."));
    let argv = build_edit_argv(&cli.agent, instruction, source, &dest)?;
    let result = execute_argv(
        cli,
        &argv,
        work_dir,
        cancel,
        child_slot,
        timeout,
        ToolPolicy::AllowWrite,
    );
    if matches!(&result, Err(error) if error.contains("cancelled") || error.contains("timed out")) {
        let _ = fs::remove_file(&dest);
        return Err(result.unwrap_err());
    }
    if dest.is_file() {
        set_progress(progress, AskProgress::Indexing);
        return image_edit::index_saved_edit(source, &dest, library_root);
    }
    match result {
        Ok(_) => Err(image_edit::no_saved_image_message(display_name(&cli.agent))),
        Err(error) => Err(error),
    }
}

fn prepare_agent_files(files: &[PathBuf]) -> Result<(tempfile::TempDir, Vec<PathBuf>), String> {
    let dir = tempfile::tempdir()
        .map_err(|error| format!("Could not create Ask AI image previews: {error}"))?;
    let mut previews = Vec::with_capacity(files.len());
    for (index, source) in files.iter().enumerate() {
        let destination = dir.path().join(format!("image-{index:03}.jpg"));
        thumbs::write_ai_preview(source, &destination).map_err(|error| {
            format!("Could not prepare {} for Ask AI: {error}", source.display())
        })?;
        previews.push(destination);
    }
    Ok((dir, previews))
}

fn execute_argv(
    cli: &AgentCli,
    argv: &[String],
    work_dir: &Path,
    cancel: &AtomicBool,
    child_slot: &Mutex<Option<Child>>,
    timeout: Duration,
    tools: ToolPolicy,
) -> Result<String, String> {
    let agent = cli.agent.as_str();
    let mut cmd = Command::new(&cli.program);
    cmd.args(&argv[1..])
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if agent == "opencode" {
        let permission = match tools {
            ToolPolicy::DenyAll => r#"{"*":"deny"}"#,
            ToolPolicy::AllowWrite => r#"{"*":"allow"}"#,
        };
        cmd.env("OPENCODE_PERMISSION", permission);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(not_installed_message(agent));
        }
        Err(e) => return Err(format!("Could not start {}: {e}", display_name(agent))),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    {
        let mut slot = child_slot.lock().unwrap_or_else(|p| p.into_inner());
        *slot = Some(child);
    }
    let stdout_h = thread::spawn(move || read_pipe(stdout));
    let stderr_h = thread::spawn(move || read_pipe(stderr));
    let started = Instant::now();
    let status = loop {
        if cancel.load(Ordering::SeqCst) {
            kill_slot(child_slot);
            let _ = stdout_h.join();
            let _ = stderr_h.join();
            reap_slot(child_slot);
            return Err("Ask AI cancelled.".into());
        }
        if started.elapsed() >= timeout {
            kill_slot(child_slot);
            let _ = stdout_h.join();
            let _ = stderr_h.join();
            reap_slot(child_slot);
            return Err("The AI request timed out.".into());
        }
        let mut slot = child_slot.lock().unwrap_or_else(|p| p.into_inner());
        match slot.as_mut().map(|c| c.try_wait()) {
            Some(Ok(Some(status))) => break status,
            Some(Ok(None)) => {}
            Some(Err(e)) => {
                drop(slot);
                return Err(format!("{} failed: {e}", display_name(agent)));
            }
            None => return Err("The AI request ended unexpectedly.".into()),
        }
        drop(slot);
        thread::sleep(POLL);
    };
    reap_slot(child_slot);
    let stdout = stdout_h.join().unwrap_or_default();
    let stderr = stderr_h.join().unwrap_or_default();
    let stdout = strip_ansi(&stdout);
    let stderr = strip_ansi(&stderr);
    if !stdout.trim().is_empty() {
        match finalize_answer(agent, &stdout) {
            Ok(answer) if status.success() => {
                if looks_like_auth_error(&answer) || looks_like_vision_error(&answer) {
                    return Err(map_error_text(agent, &answer));
                }
                return Ok(answer);
            }
            Ok(answer) => {
                let error = combine_output(&answer, &stderr);
                return Err(map_error_text(agent, &error));
            }
            Err(error) => {
                let error = combine_output(&error, &stderr);
                return Err(map_error_text(agent, &error));
            }
        }
    }
    let combined = if stderr.trim().is_empty() {
        stdout
    } else if stdout.trim().is_empty() {
        stderr
    } else {
        format!("{}\n{}", stdout.trim(), stderr.trim())
    };
    Err(map_error_text(agent, &combined))
}

fn finalize_answer(agent: &str, stdout: &str) -> Result<String, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(format!("{} returned no answer.", display_name(agent)));
    }
    Ok(finalize_plain_answer(trimmed))
}

fn finalize_plain_answer(stdout: &str) -> String {
    let paragraphs: Vec<&str> = stdout
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .collect();
    match paragraphs.as_slice() {
        [] => stdout.trim().to_string(),
        [only] => (*only).to_string(),
        _ => paragraphs.last().copied().unwrap_or("").to_string(),
    }
}

fn combine_output(primary: &str, secondary: &str) -> String {
    match (primary.trim(), secondary.trim()) {
        ("", secondary) => secondary.to_string(),
        (primary, "") => primary.to_string(),
        (primary, secondary) => format!("{primary}\n{secondary}"),
    }
}

fn read_pipe(pipe: Option<impl Read>) -> String {
    let mut buf = String::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_string(&mut buf);
    }
    buf
}

fn reap_slot(slot: &Mutex<Option<Child>>) {
    if let Ok(mut guard) = slot.lock() {
        if let Some(mut child) = guard.take() {
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn photo(rel: &str) -> Photo {
        Photo {
            relpath: rel.into(),
            album: "album".into(),
            filename: rel.into(),
            mtime: 0,
            size: 0,
            captured_at: None,
            camera: None,
            width: None,
            height: None,
        }
    }

    #[test]
    fn os_release_detects_omarchy_id_only() {
        assert!(os_release_is_omarchy(
            "NAME=Omarchy\nID=omarchy\nID_LIKE=arch\n"
        ));
        assert!(os_release_is_omarchy("ID=\"omarchy\"\n"));
        assert!(!os_release_is_omarchy("ID=arch\nID_LIKE=arch\n"));
        assert!(!os_release_is_omarchy("NAME=Omarchy\nID=arch\n"));
    }

    #[test]
    fn omarchy_detection_requires_linux() {
        assert!(!is_omarchy_from(false, Some("ID=omarchy\n"), true));
        assert!(is_omarchy_from(true, Some("ID=omarchy\n"), false));
        assert!(is_omarchy_from(true, Some("ID=arch\n"), true));
        assert!(!is_omarchy_from(true, Some("ID=arch\n"), false));
        assert!(!is_omarchy_from(true, None, false));
    }

    #[test]
    fn agent_file_aliases_fold() {
        assert_eq!(parse_agent_file("opencode\n"), Some("opencode".into()));
        assert_eq!(parse_agent_file("open-code"), Some("opencode".into()));
        assert_eq!(parse_agent_file("oh-my-pi\n"), Some("omp".into()));
        assert_eq!(parse_agent_file("claude-code"), Some("claude".into()));
        assert_eq!(parse_agent_file("gemini"), Some("agy".into()));
        assert_eq!(parse_agent_file(""), None);
        assert_eq!(parse_agent_file("\n"), None);
    }

    #[test]
    fn marked_stills_keep_album_order_and_drop_video() {
        let photos = vec![
            photo("a.jpg"),
            photo("clip.mov"),
            photo("b.PNG"),
            photo("c.mp4"),
            photo("d.webp"),
        ];
        let marked = HashSet::from([
            "d.webp".into(),
            "clip.mov".into(),
            "a.jpg".into(),
            "c.mp4".into(),
        ]);
        assert_eq!(marked_still_rels(&photos, &marked), vec!["a.jpg", "d.webp"]);
        assert!(marked_still_rels(&photos, &HashSet::from(["clip.mov".into()])).is_empty());
    }

    #[test]
    fn ask_ai_active_needs_a_resolved_agent_and_stills() {
        assert!(!ask_ai_active(None, &["a.jpg".into()]));
        assert!(!ask_ai_active(Some("opencode"), &[]));
        assert!(ask_ai_active(Some("opencode"), &["a.jpg".into()]));
    }

    #[test]
    fn omarchy_default_beats_path_auto_detect() {
        let input = ResolveInput {
            allow_path: true,
            omarchy_default: Some("claude".into()),
            real_on_path: vec!["opencode".into(), "claude".into()],
        };
        assert_eq!(resolve_agent_from(&input).as_deref(), Some("claude"));
    }

    #[test]
    fn empty_omarchy_default_falls_back_to_path_order() {
        let input = ResolveInput {
            allow_path: true,
            omarchy_default: None,
            real_on_path: vec!["claude".into(), "opencode".into()],
        };
        assert_eq!(resolve_agent_from(&input).as_deref(), Some("opencode"));
        let none = ResolveInput {
            allow_path: true,
            omarchy_default: Some("  ".into()),
            real_on_path: vec!["pi".into()],
        };
        assert_eq!(resolve_agent_from(&none).as_deref(), Some("pi"));
    }

    #[test]
    fn path_detect_skips_when_platform_disallows() {
        let input = ResolveInput {
            allow_path: false,
            omarchy_default: None,
            real_on_path: vec!["opencode".into()],
        };
        assert_eq!(resolve_agent_from(&input), None);
        assert!(!ask_platform_ok(false, false));
        assert!(ask_platform_ok(true, false));
        assert!(ask_platform_ok(false, true));
    }

    #[test]
    fn macos_shaped_path_detect_picks_opencode() {
        let input = ResolveInput {
            allow_path: ask_platform_ok(true, false),
            omarchy_default: None,
            real_on_path: vec!["opencode".into()],
        };
        assert_eq!(resolve_agent_from(&input).as_deref(), Some("opencode"));
        assert!(ask_ai_active(
            resolve_agent_from(&input).as_deref(),
            &["a.jpg".into()]
        ));
    }

    #[test]
    fn stub_skip_uses_next_real_binary() {
        let stub =
            b"#!/bin/bash\nmise use -g opencode\nexec mise exec opencode -- opencode \"$@\"\n";
        assert!(looks_like_omarchy_stub(stub));
        assert!(looks_like_omarchy_stub(
            b"#!/bin/sh\nomarchy-cmd-missing claude\n"
        ));
        assert!(looks_like_omarchy_stub(
            b"#!/bin/sh\nomarchy-install hermes\n"
        ));
        assert!(!looks_like_omarchy_stub(b"\x7fELFnot-a-stub"));
        assert!(!looks_like_omarchy_stub(&[0xfe, 0xed, 0xfa, 0xce, b'x']));
        let input = ResolveInput {
            allow_path: true,
            omarchy_default: None,
            real_on_path: vec!["claude".into()],
        };
        assert_eq!(resolve_agent_from(&input).as_deref(), Some("claude"));
    }

    #[test]
    fn unsupported_omarchy_default_still_resolves() {
        let input = ResolveInput {
            allow_path: true,
            omarchy_default: Some("ori".into()),
            real_on_path: vec!["opencode".into()],
        };
        assert_eq!(resolve_agent_from(&input).as_deref(), Some("ori"));
        let err = build_argv("ori", "x", &[PathBuf::from("a.jpg")]).unwrap_err();
        assert!(err.contains("ori"), "{err}");
    }

    #[test]
    fn opencode_argv_matches_requested_shape() {
        let files = [PathBuf::from("/lib/a.png"), PathBuf::from("/lib/b.jpg")];
        let argv = build_argv("opencode", "what is this?", &files).unwrap();
        assert!(argv[2].contains("direct final answer"));
        assert!(argv[2].contains("{\"edit\":\""));
        assert!(argv[2].ends_with("User request:\nwhat is this?"));
        assert_eq!(&argv[..2], ["opencode", "run"]);
        let cwd = env::current_dir().unwrap();
        assert_eq!(
            &argv[3..],
            [
                "--dir",
                cwd.to_str().unwrap(),
                "-f",
                "/lib/a.png",
                "-f",
                "/lib/b.jpg"
            ]
        );
        assert_ne!(argv[4], "/lib", "preview/album dirs must not become --dir");
    }

    #[test]
    fn finalize_plain_answer_keeps_a_single_paragraph() {
        assert_eq!(
            finalize_plain_answer("Salta, Argentina"),
            "Salta, Argentina"
        );
    }

    #[test]
    fn finalize_plain_answer_keeps_the_last_paragraph() {
        let text = "Let me inspect the image.\n\nSalta, Argentina";
        assert_eq!(finalize_plain_answer(text), "Salta, Argentina");
    }

    #[test]
    fn finalize_answer_plain_agents_drop_leading_reasoning() {
        let text = "I'll look at the photo.\n\nA red car.";
        for agent in ["pi", "omp", "hermes", "codex", "claude", "opencode"] {
            assert_eq!(
                finalize_answer(agent, text).unwrap(),
                "A red car.",
                "{agent}"
            );
        }
    }

    #[test]
    fn all_agents_receive_photo_qa_prompt() {
        let files = [PathBuf::from("image-000.jpg")];
        for agent in ["pi", "omp", "hermes", "codex", "claude", "opencode"] {
            let argv = build_argv(agent, "what car?", &files).unwrap();
            let prompt = match agent {
                "opencode" => &argv[2],
                "claude" => argv.last().unwrap(),
                "codex" => &argv[5],
                "hermes" => argv.last().unwrap(),
                _ => argv.last().unwrap(),
            };
            assert!(prompt.contains("direct final answer"), "{agent}");
            assert!(prompt.contains("{\"edit\":\""), "{agent}");
            assert!(prompt.ends_with("User request:\nwhat car?"), "{agent}");
        }
    }

    #[test]
    fn marked_stills_become_metadata_free_jpeg_previews() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("photo.png");
        image::RgbImage::from_pixel(40, 20, image::Rgb([20, 40, 60]))
            .save(&source)
            .unwrap();

        let (_preview_dir, files) = prepare_agent_files(&[source]).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].file_name().and_then(|name| name.to_str()),
            Some("image-000.jpg")
        );
        assert_eq!(
            files[0].extension().and_then(|ext| ext.to_str()),
            Some("jpg")
        );
        assert_eq!(image::image_dimensions(&files[0]).unwrap(), (40, 20));
    }

    #[test]
    fn large_stills_are_downscaled_for_ask_ai() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("large.png");
        image::RgbImage::from_pixel(3200, 2400, image::Rgb([10, 20, 30]))
            .save(&source)
            .unwrap();

        let (_preview_dir, files) = prepare_agent_files(&[source]).unwrap();

        let (width, height) = image::image_dimensions(&files[0]).unwrap();
        assert!(width <= thumbs::AI_PREVIEW_MAX_SIZE);
        assert!(height <= thumbs::AI_PREVIEW_MAX_SIZE);
    }

    #[test]
    fn pi_argv_attaches_at_files_before_prompt() {
        let files = [PathBuf::from("/p/one.jpg")];
        let argv = build_argv("pi", "describe", &files).unwrap();
        assert_eq!(
            argv,
            vec![
                "pi",
                "-p",
                "--no-tools",
                "--no-context-files",
                "--no-session",
                "@one.jpg",
                "--",
                &photo_qa_prompt("describe"),
            ]
        );
    }

    #[test]
    fn omp_argv_uses_print_and_at_files() {
        let files = [PathBuf::from("shot.png")];
        let argv = build_argv("omp", "hi", &files).unwrap();
        assert_eq!(
            argv,
            vec![
                "omp",
                "-p",
                "--no-tools",
                "--no-session",
                "@shot.png",
                "--",
                &photo_qa_prompt("hi"),
            ]
        );
    }

    #[test]
    fn hermes_argv_repeats_image_flags() {
        let files = [PathBuf::from("a.jpg"), PathBuf::from("b.jpg")];
        let argv = build_argv("hermes", "compare", &files).unwrap();
        assert_eq!(
            argv,
            vec![
                "hermes",
                "chat",
                "--oneshot",
                "-Q",
                "--safe-mode",
                "--image",
                "a.jpg",
                "--image",
                "b.jpg",
                "-q",
                &photo_qa_prompt("compare"),
            ]
        );
    }

    #[test]
    fn codex_prompt_precedes_image_flags() {
        let files = [PathBuf::from("a.png")];
        let argv = build_argv("codex", "look", &files).unwrap();
        assert_eq!(
            argv,
            vec![
                "codex",
                "exec",
                "--ephemeral",
                "--sandbox",
                "read-only",
                &photo_qa_prompt("look"),
                "-i",
                "a.png",
            ]
        );
        let prompt_at = argv
            .iter()
            .position(|a| a.contains("User request:\nlook"))
            .unwrap();
        let image_at = argv.iter().position(|a| a == "-i").unwrap();
        assert!(prompt_at < image_at);
    }

    #[test]
    fn claude_argv_embeds_preview_names_and_read_only_tools() {
        let files = [PathBuf::from("/tmp/ask-ai/image-000.jpg")];
        let argv = build_argv("claude", "caption", &files).unwrap();
        assert_eq!(
            &argv[..6],
            [
                "claude",
                "-p",
                "--tools",
                "Read",
                "--permission-mode",
                "dontAsk"
            ]
        );
        assert!(argv[7].contains("image-000.jpg"));
        assert!(!argv[7].contains("/tmp/ask-ai"));
        assert!(argv[7].contains("caption"));
    }

    #[test]
    fn unsupported_agents_get_a_helpful_message() {
        for agent in ["ori", "crush", "grok", "agy", "copilot"] {
            let err = build_argv(agent, "x", &[PathBuf::from("a.jpg")]).unwrap_err();
            assert!(err.contains(agent), "{err}");
            assert!(err.contains("does not yet support"), "{err}");
        }
        assert_eq!(canonicalize_agent(""), None);
        assert!(no_agent_message_for(false).contains("PATH"));
        assert!(no_agent_message_for(true).contains("omarchy default agent"));
    }

    #[test]
    fn empty_file_list_is_a_video_error() {
        assert_eq!(
            build_argv("opencode", "x", &[]).unwrap_err(),
            no_images_message()
        );
    }

    #[test]
    fn auth_and_vision_errors_are_mapped() {
        let auth = map_error_text("opencode", "AI_APICallError: No auth credentials found");
        assert!(auth.contains("opencode auth login"), "{auth}");
        let vision = map_error_text("pi", "model does not support image input");
        assert!(vision.contains("cannot read images"), "{vision}");
        let other = map_error_text("codex", "rate limit exceeded");
        assert_eq!(other, "rate limit exceeded");
    }

    #[test]
    fn strip_ansi_drops_color_and_keeps_text() {
        assert_eq!(strip_ansi("\u{1b}[31mhello\u{1b}[0m"), "hello");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn waiting_dots_cycle_through_three_then_none() {
        let t0 = Instant::now();
        assert_eq!(
            waiting_text(AskProgress::Analyzing, t0, t0),
            "Analyzing prompt"
        );
        assert_eq!(
            waiting_text(AskProgress::Analyzing, t0, t0 + Duration::from_millis(400)),
            "Analyzing prompt."
        );
        assert_eq!(
            waiting_text(AskProgress::Editing, t0, t0 + Duration::from_millis(800)),
            "Editing image.."
        );
        assert_eq!(
            waiting_text(AskProgress::Indexing, t0, t0 + Duration::from_millis(1200)),
            "Indexing result..."
        );
        assert_eq!(
            waiting_text(AskProgress::Analyzing, t0, t0 + Duration::from_millis(1600)),
            "Analyzing prompt"
        );
    }

    #[test]
    fn pane_grows_for_a_second_paragraph_and_caps() {
        assert_eq!(search_pane_height(false, "q", None, 40), 3);
        assert_eq!(search_pane_height(true, "hi", None, 40), 3);
        let h = search_pane_height(true, "hi", Some("Waiting..."), 40);
        assert!(h >= 5);
        let long = "word ".repeat(400);
        assert_eq!(search_pane_height(true, "p", Some(&long), 40), ASK_PANE_MAX);
    }

    #[test]
    fn execute_echo_returns_stdout() {
        let cancel = AtomicBool::new(false);
        let slot = Mutex::new(None);
        let argv = vec!["pi".into(), "hello-ask".into()];
        let out = execute_argv(
            &AgentCli::with_program("pi", "echo"),
            &argv,
            Path::new("."),
            &cancel,
            &slot,
            Duration::from_secs(5),
            ToolPolicy::DenyAll,
        )
        .unwrap();
        assert_eq!(out.trim(), "hello-ask");
    }

    #[test]
    fn execute_nonzero_maps_stderr() {
        let cancel = AtomicBool::new(false);
        let slot = Mutex::new(None);
        let argv = vec![
            "opencode".into(),
            "-c".into(),
            "echo 'No auth credentials found' >&2; exit 1".into(),
        ];
        let err = execute_argv(
            &AgentCli::with_program("opencode", "sh"),
            &argv,
            Path::new("."),
            &cancel,
            &slot,
            Duration::from_secs(5),
            ToolPolicy::DenyAll,
        )
        .unwrap_err();
        assert!(err.contains("opencode auth login"), "{err}");
    }

    #[test]
    fn cancel_kills_a_sleeping_child() {
        let mut handle = {
            let (tx, rx) = mpsc::channel();
            let cancel = Arc::new(AtomicBool::new(false));
            let child_slot = Arc::new(Mutex::new(None));
            let cancel_t = cancel.clone();
            let slot_t = child_slot.clone();
            thread::spawn(move || {
                let result = execute_argv(
                    &AgentCli::with_program("pi", "sleep"),
                    &["pi".into(), "30".into()],
                    Path::new("."),
                    &cancel_t,
                    &slot_t,
                    Duration::from_secs(30),
                    ToolPolicy::DenyAll,
                )
                .map(AskValue::Answer);
                let _ = tx.send(AskOutcome { id: 1, result });
            });
            AskHandle {
                id: 1,
                rx,
                cancel,
                child_slot,
                progress: Arc::new(Mutex::new(AskProgress::Analyzing)),
            }
        };
        thread::sleep(Duration::from_millis(80));
        handle.cancel();
        let started = Instant::now();
        let outcome = loop {
            if let Some(o) = handle.try_recv() {
                break o;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "cancel did not finish"
            );
            thread::sleep(Duration::from_millis(20));
        };
        assert!(outcome.result.unwrap_err().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn timeout_kills_a_sleeping_child() {
        let cancel = AtomicBool::new(false);
        let slot = Mutex::new(None);
        let started = Instant::now();
        let err = execute_argv(
            &AgentCli::with_program("pi", "sleep"),
            &["pi".into(), "30".into()],
            Path::new("."),
            &cancel,
            &slot,
            Duration::from_millis(200),
            ToolPolicy::DenyAll,
        )
        .unwrap_err();
        assert_eq!(err, "The AI request timed out.");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_missing_program_is_reported_as_not_installed() {
        let cancel = AtomicBool::new(false);
        let slot = Mutex::new(None);
        let dir = tempfile::tempdir().unwrap();
        let err = execute_argv(
            &AgentCli::with_program("opencode", dir.path().join("absent")),
            &["opencode".into()],
            dir.path(),
            &cancel,
            &slot,
            Duration::from_secs(5),
            ToolPolicy::DenyAll,
        )
        .unwrap_err();
        assert!(err.contains("not installed"), "{err}");
    }

    #[test]
    fn parse_ask_decision_accepts_bare_edit_json() {
        assert_eq!(
            parse_ask_decision(r#"{"edit":"Remove persons in the background."}"#),
            AskDecision::Edit("Remove persons in the background.".into())
        );
    }

    #[test]
    fn parse_ask_decision_accepts_fenced_edit_json() {
        let text = "```json\n{\"edit\":\"Blur the background\"}\n```";
        assert_eq!(
            parse_ask_decision(text),
            AskDecision::Edit("Blur the background".into())
        );
    }

    #[test]
    fn parse_ask_decision_treats_malformed_or_prose_as_answer() {
        assert_eq!(
            parse_ask_decision("A red car."),
            AskDecision::Answer("A red car.".into())
        );
        assert_eq!(
            parse_ask_decision(r#"Sure. {"edit":"remove them"}"#),
            AskDecision::Answer(r#"Sure. {"edit":"remove them"}"#.into())
        );
        assert_eq!(
            parse_ask_decision(r#"{"edit":""}"#),
            AskDecision::Answer(r#"{"edit":""}"#.into())
        );
        assert_eq!(
            parse_ask_decision(r#"{"action":"edit","instruction":"remove them"}"#),
            AskDecision::Answer(r#"{"action":"edit","instruction":"remove them"}"#.into())
        );
    }

    #[test]
    fn edit_json_from_agent_with_multiple_files_is_rejected() {
        assert!(edit_source_count_ok(1).is_ok());
        assert_eq!(
            edit_source_count_ok(2).unwrap_err(),
            image_edit::edit_needs_one_photo_message()
        );
        assert_eq!(
            edit_source_count_ok(0).unwrap_err(),
            "Image editing needs exactly one marked photo."
        );
    }

    #[test]
    fn edit_argv_asks_the_agent_to_write_a_sibling() {
        let source = PathBuf::from("/lib/Rome/photo.jpg");
        let dest = PathBuf::from("/lib/Rome/photo-edited.png");
        for agent in SUPPORTED {
            let argv = build_edit_argv(agent, "Remove the people", &source, &dest).unwrap();
            assert!(
                !argv.iter().any(|arg| arg == "--model"),
                "{agent} must keep the user's configured model"
            );
            assert!(
                argv.iter().any(|arg| arg.contains("photo-edited.png")),
                "{agent}"
            );
        }

        let argv = build_edit_argv("opencode", "Remove the people", &source, &dest).unwrap();
        assert_eq!(&argv[..2], ["opencode", "run"]);
        let cwd = env::current_dir().unwrap();
        assert_eq!(
            &argv[3..],
            ["--dir", cwd.to_str().unwrap(), "-f", "/lib/Rome/photo.jpg"]
        );

        let pi = build_edit_argv("pi", "blur", &source, &dest).unwrap();
        assert!(!pi.iter().any(|arg| arg == "--no-tools"));
        assert!(pi.iter().any(|arg| arg == "@photo.jpg"));

        let hermes = build_edit_argv("hermes", "blur", &source, &dest).unwrap();
        assert!(!hermes.iter().any(|arg| arg == "--safe-mode"));

        let codex = build_edit_argv("codex", "blur", &source, &dest).unwrap();
        assert!(codex.iter().any(|arg| arg == "workspace-write"));
        assert!(!codex.iter().any(|arg| arg == "read-only"));

        let claude = build_edit_argv("claude", "blur", &source, &dest).unwrap();
        assert_eq!(claude[3], "Read,Write");
    }

    #[test]
    fn cancel_before_edit_does_not_write() {
        let cancel = AtomicBool::new(true);
        let slot = Mutex::new(None);
        let progress = Mutex::new(AskProgress::Editing);
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.jpg");
        fs::write(&source, b"orig").unwrap();
        let err = run_agent_edit(
            &AgentCli::on_path("opencode"),
            &source,
            dir.path(),
            "remove people",
            &cancel,
            &slot,
            &progress,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(!dir.path().join("photo-edited.png").exists());
    }
}
