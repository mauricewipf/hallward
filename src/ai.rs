//! Headless Ask AI: classify and answer with OpenRouter, then edit stills if needed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::catalog::Photo;
use crate::credentials::{self, ResolvedKey};
use crate::image_edit::{self, PostFn, SavedEdit};
use crate::media::is_image;
use crate::thumbs;

pub const ASK_TIMEOUT: Duration = Duration::from_secs(120);
const DOT_STEP: Duration = Duration::from_millis(400);

/// Progress shown while a headless Ask AI / edit job is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskProgress {
    Analyzing,
    Editing,
    Indexing,
}

/// Successful Ask AI outcome: a text answer or a newly saved edited still.
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

/// Wall-clock budgets for the classify hop and the optional image-edit hop.
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

/// Handle for an in-flight Ask AI request.
pub struct AskHandle {
    pub id: u64,
    rx: Receiver<AskOutcome>,
    cancel: Arc<AtomicBool>,
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

pub fn ask_ai_active(stills: &[String]) -> bool {
    !stills.is_empty()
}

pub fn waiting_text(phase: AskProgress, started: Instant, now: Instant) -> String {
    let base = match phase {
        AskProgress::Analyzing => "Waiting",
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

pub fn no_images_message() -> String {
    "Videos can't be sent to the AI. Mark a photo and try again.".into()
}

pub(crate) fn photo_qa_prompt(user_prompt: &str) -> String {
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

/// Spawn a headless Ask AI request on a background thread.
pub fn spawn(
    id: u64,
    prompt: String,
    files: Vec<PathBuf>,
    library_root: PathBuf,
    key: ResolvedKey,
) -> AskHandle {
    spawn_with(
        id,
        prompt,
        files,
        library_root,
        key,
        Timeouts::default(),
        image_edit::post_openrouter,
    )
}

/// [`spawn`] with an injectable HTTP transport and timeouts for tests.
pub fn spawn_with(
    id: u64,
    prompt: String,
    files: Vec<PathBuf>,
    library_root: PathBuf,
    key: ResolvedKey,
    timeouts: Timeouts,
    post: PostFn,
) -> AskHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(AskProgress::Analyzing));
    let cancel_t = cancel.clone();
    let progress_t = progress.clone();
    thread::spawn(move || {
        let result = run_ask(
            &prompt,
            &files,
            &library_root,
            &key,
            &cancel_t,
            &progress_t,
            post,
            timeouts,
        );
        let _ = tx.send(AskOutcome { id, result });
    });
    AskHandle {
        id,
        rx,
        cancel,
        progress,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_ask(
    prompt: &str,
    files: &[PathBuf],
    library_root: &Path,
    key: &ResolvedKey,
    cancel: &AtomicBool,
    progress: &Mutex<AskProgress>,
    post: PostFn,
    timeouts: Timeouts,
) -> Result<AskValue, String> {
    if cancel.load(Ordering::SeqCst) {
        return Err("Ask AI cancelled.".into());
    }
    if files.is_empty() {
        return Err(no_images_message());
    }
    set_progress(progress, AskProgress::Analyzing);
    let (_preview_dir, previews) = prepare_ask_files(files)?;
    let answer = match image_edit::run_ask(
        &previews,
        &photo_qa_prompt(prompt),
        key,
        cancel,
        post,
        timeouts.ask,
    ) {
        Ok(text) => text,
        Err(err) => return map_job_error(err),
    };
    match parse_ask_decision(&finalize_plain_answer(&answer)) {
        AskDecision::Answer(text) => {
            if text.is_empty() {
                Err("OpenRouter returned no answer.".into())
            } else {
                Ok(AskValue::Answer(text))
            }
        }
        AskDecision::Edit(instruction) => {
            edit_source_count_ok(files.len())?;
            set_progress(progress, AskProgress::Editing);
            match image_edit::run_edit(
                &files[0],
                library_root,
                &instruction,
                key,
                cancel,
                post,
                timeouts.edit,
            ) {
                Ok(saved) => {
                    set_progress(progress, AskProgress::Indexing);
                    Ok(AskValue::Saved(saved))
                }
                Err(err) => map_job_error(err),
            }
        }
    }
}

fn map_job_error(err: String) -> Result<AskValue, String> {
    if credentials::is_invalid_saved_key_error(&err) {
        let _ = credentials::clear();
    }
    Err(err)
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

fn prepare_ask_files(files: &[PathBuf]) -> Result<(tempfile::TempDir, Vec<PathBuf>), String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;

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
            raw_relpath: None,
        }
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
    fn ask_ai_active_needs_marked_stills() {
        assert!(!ask_ai_active(&[]));
        assert!(ask_ai_active(&["a.jpg".into()]));
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
    fn empty_file_list_is_a_video_error() {
        let cancel = AtomicBool::new(false);
        let progress = Mutex::new(AskProgress::Analyzing);
        let key = ResolvedKey::new("test", credentials::CredentialSource::File);
        let err = run_ask(
            "which car?",
            &[],
            Path::new("."),
            &key,
            &cancel,
            &progress,
            |_url, _api_key, _body, _timeout| Ok((200, "{}".into())),
            Timeouts::default(),
        )
        .unwrap_err();
        assert_eq!(err, no_images_message());
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
    fn waiting_dots_cycle_through_three_then_none() {
        let t0 = Instant::now();
        assert_eq!(waiting_text(AskProgress::Analyzing, t0, t0), "Waiting");
        assert_eq!(
            waiting_text(AskProgress::Analyzing, t0, t0 + Duration::from_millis(400)),
            "Waiting."
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
            "Waiting"
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
    fn marked_stills_become_metadata_free_jpeg_previews() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("photo.png");
        image::RgbImage::from_pixel(40, 20, image::Rgb([20, 40, 60]))
            .save(&source)
            .unwrap();

        let (_preview_dir, files) = prepare_ask_files(&[source]).unwrap();

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

        let (_preview_dir, files) = prepare_ask_files(&[source]).unwrap();

        let (width, height) = image::image_dimensions(&files[0]).unwrap();
        assert!(width <= thumbs::AI_PREVIEW_MAX_SIZE);
        assert!(height <= thumbs::AI_PREVIEW_MAX_SIZE);
    }

    #[test]
    fn photo_qa_prompt_asks_for_edit_json() {
        let prompt = photo_qa_prompt("what car?");
        assert!(prompt.contains("direct final answer"));
        assert!(prompt.contains("{\"edit\":\""));
        assert!(prompt.ends_with("User request:\nwhat car?"));
    }

    #[test]
    fn cancel_before_ask_does_not_call_openrouter() {
        let cancel = AtomicBool::new(true);
        let progress = Mutex::new(AskProgress::Analyzing);
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.jpg");
        fs::write(&source, b"orig").unwrap();
        let key = ResolvedKey::new("test", credentials::CredentialSource::File);
        let err = run_ask(
            "which car?",
            &[source],
            dir.path(),
            &key,
            &cancel,
            &progress,
            |_url, _api_key, _body, _timeout| panic!("OpenRouter must not be called after cancel"),
            Timeouts::default(),
        )
        .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
    }
}
