use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use rayon::prelude::*;

use crate::media::{is_heic, is_image, is_video};

const OPEN_BIN: &str = "/usr/bin/open";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewer {
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Imv,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Nsxiv,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Feh,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Swayimg,
    Preview,
}

impl Viewer {
    pub fn bin(self) -> &'static str {
        match self {
            Viewer::Imv => "imv",
            Viewer::Nsxiv => "nsxiv",
            Viewer::Feh => "feh",
            Viewer::Swayimg => "swayimg",
            Viewer::Preview => "Preview",
        }
    }

    fn spawn_name(self) -> &'static str {
        match self {
            Viewer::Preview => "open",
            _ => self.bin(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPlayer {
    Mpv,
    Ffplay,
}

impl VideoPlayer {
    pub fn bin(self) -> &'static str {
        match self {
            VideoPlayer::Mpv => "mpv",
            VideoPlayer::Ffplay => "ffplay",
        }
    }
}

pub fn detect() -> Option<Viewer> {
    #[cfg(target_os = "macos")]
    {
        if Path::new(OPEN_BIN).is_file() {
            return Some(Viewer::Preview);
        }
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        [Viewer::Imv, Viewer::Nsxiv, Viewer::Feh, Viewer::Swayimg]
            .into_iter()
            .find(|&v| on_path(v.bin()))
    }
}

pub fn detect_video_player() -> Option<VideoPlayer> {
    [VideoPlayer::Mpv, VideoPlayer::Ffplay]
        .into_iter()
        .find(|&v| on_path(v.bin()))
}

fn on_path(bin: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Argument vector (including argv0) to open `files` starting at `start` (0-based).
/// Swayimg has no start-at flag: always starts at the first file.
/// Preview uses `/usr/bin/open` and puts the selected file first (no start-at flag).
pub fn argv(viewer: Viewer, files: &[PathBuf], start: usize) -> Vec<OsString> {
    if viewer == Viewer::Preview {
        return preview_argv(files, start);
    }
    let mut args = vec![OsString::from(viewer.bin())];
    if files.is_empty() {
        return args;
    }
    let start = start.min(files.len() - 1);
    match viewer {
        Viewer::Preview => unreachable!(),
        Viewer::Imv => {
            args.push("-n".into());
            args.push(files[start].clone().into());
            args.extend(files.iter().map(|p| p.clone().into()));
        }
        Viewer::Nsxiv => {
            args.push("-n".into());
            args.push(OsString::from((start + 1).to_string()));
            args.extend(files.iter().map(|p| p.clone().into()));
        }
        Viewer::Feh => {
            args.push("--start-at".into());
            args.push(files[start].clone().into());
            args.extend(files.iter().map(|p| p.clone().into()));
        }
        Viewer::Swayimg => {
            args.extend(files.iter().map(|p| p.clone().into()));
        }
    }
    args
}

fn preview_argv(files: &[PathBuf], start: usize) -> Vec<OsString> {
    if files.is_empty() {
        return vec![OsString::from(OPEN_BIN)];
    }
    let start = start.min(files.len() - 1);
    let mut args: Vec<OsString> = [OPEN_BIN, "-n", "-W", "-a", "Preview", "--"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(files[start].clone().into());
    args.extend(
        files
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != start)
            .map(|(_, p)| p.clone().into()),
    );
    args
}

/// mpv `--playlist-start` is zero-based.
pub fn mpv_argv(files: &[PathBuf], start: usize) -> Vec<OsString> {
    let mut args = vec![OsString::from("mpv")];
    if files.is_empty() {
        return args;
    }
    let start = start.min(files.len() - 1);
    args.push(OsString::from("--autofit-larger=100%x100%"));
    args.push(OsString::from(format!("--playlist-start={start}")));
    args.extend(files.iter().map(|p| p.clone().into()));
    args
}

pub fn ffplay_argv(file: &Path) -> Vec<OsString> {
    vec![OsString::from("ffplay"), file.as_os_str().to_os_string()]
}

/// Keep files of the same media type as `files[start]`, remap the start index.
pub fn same_type_playlist(files: &[PathBuf], start: usize) -> (Vec<PathBuf>, usize) {
    if files.is_empty() {
        return (Vec::new(), 0);
    }
    let start = start.min(files.len() - 1);
    let selected = &files[start];
    let want_video = is_video(selected);
    let playlist: Vec<PathBuf> = files
        .iter()
        .filter(|p| if want_video { is_video(p) } else { is_image(p) })
        .cloned()
        .collect();
    let new_start = playlist.iter().position(|p| p == selected).unwrap_or(0);
    (playlist, new_start)
}

pub fn open(files: &[PathBuf], start: usize) -> Result<()> {
    if files.is_empty() {
        bail!("no files to open");
    }
    let (playlist, start) = same_type_playlist(files, start);
    if playlist.is_empty() {
        bail!("no files to open");
    }
    if is_video(&playlist[start]) {
        open_video(&playlist, start)
    } else {
        open_images(&playlist, start)
    }
}

fn open_images(files: &[PathBuf], start: usize) -> Result<()> {
    let Some(viewer) = detect() else {
        #[cfg(target_os = "macos")]
        bail!("no image viewer found (Preview requires /usr/bin/open)");
        #[cfg(not(target_os = "macos"))]
        bail!("no image viewer found (tried imv, nsxiv, feh, swayimg)");
    };
    if viewer == Viewer::Imv && files.iter().any(|path| is_heic(path)) {
        let (_temp, files) = prepare_heif_for_imv(files)?;
        return spawn(&argv(viewer, &files, start), viewer.spawn_name());
    }
    spawn(&argv(viewer, files, start), viewer.spawn_name())
}

fn prepare_heif_for_imv(files: &[PathBuf]) -> Result<(tempfile::TempDir, Vec<PathBuf>)> {
    let temp = tempfile::tempdir().context("create temporary HEIF viewer directory")?;
    let dir = temp.path();
    let prepared = files
        .par_iter()
        .enumerate()
        .map(|(index, path)| {
            if !is_heic(path) {
                return Ok(path.clone());
            }
            let output = dir.join(format!("{index:06}.jpg"));
            let status = Command::new("heif-convert")
                .args(["--quiet", "--quality", "100"])
                .arg(path)
                .arg(&output)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .with_context(|| format!("convert {} for imv", path.display()))?;
            if !status.success() {
                bail!("heif-convert failed on {}", path.display());
            }
            Ok(output)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((temp, prepared))
}

fn open_video(files: &[PathBuf], start: usize) -> Result<()> {
    match detect_video_player() {
        Some(VideoPlayer::Mpv) => spawn(&mpv_argv(files, start), "mpv"),
        Some(VideoPlayer::Ffplay) => spawn(&ffplay_argv(&files[start]), "ffplay"),
        None => bail!("no video player found (tried mpv, ffplay)"),
    }
}

fn spawn(args: &[OsString], name: &str) -> Result<()> {
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.status().with_context(|| format!("spawn {name}"))?;
    Ok(())
}

pub fn abs_files(root: &Path, rels: &[String]) -> Vec<PathBuf> {
    rels.iter().map(|r| root.join(r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/lib/a.jpg"),
            PathBuf::from("/lib/b.jpg"),
            PathBuf::from("/lib/c.jpg"),
        ]
    }

    fn mixed() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/lib/a.jpg"),
            PathBuf::from("/lib/clip.mov"),
            PathBuf::from("/lib/b.jpg"),
            PathBuf::from("/lib/other.mp4"),
        ]
    }

    fn os(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn imv_starts_at_selected_path() {
        let a = os(&argv(Viewer::Imv, &files(), 1));
        assert_eq!(
            a,
            vec![
                "imv",
                "-n",
                "/lib/b.jpg",
                "/lib/a.jpg",
                "/lib/b.jpg",
                "/lib/c.jpg"
            ]
        );
    }

    #[test]
    fn nsxiv_uses_one_based_index() {
        let a = os(&argv(Viewer::Nsxiv, &files(), 2));
        assert_eq!(a[0], "nsxiv");
        assert_eq!(a[1], "-n");
        assert_eq!(a[2], "3");
    }

    #[test]
    fn feh_start_at() {
        let a = os(&argv(Viewer::Feh, &files(), 0));
        assert_eq!(a[1], "--start-at");
        assert_eq!(a[2], "/lib/a.jpg");
    }

    #[test]
    fn swayimg_starts_at_first_no_workaround() {
        let a = os(&argv(Viewer::Swayimg, &files(), 2));
        assert_eq!(a, vec!["swayimg", "/lib/a.jpg", "/lib/b.jpg", "/lib/c.jpg"]);
    }

    #[test]
    fn preview_starts_at_selected_path() {
        let a = os(&argv(Viewer::Preview, &files(), 1));
        assert_eq!(
            a,
            vec![
                "/usr/bin/open",
                "-n",
                "-W",
                "-a",
                "Preview",
                "--",
                "/lib/b.jpg",
                "/lib/a.jpg",
                "/lib/c.jpg",
            ]
        );
    }

    #[test]
    fn preview_empty_files_is_open_only() {
        let a = os(&argv(Viewer::Preview, &[], 0));
        assert_eq!(a, vec!["/usr/bin/open"]);
    }

    #[test]
    fn mpv_playlist_start_is_zero_based() {
        let a = os(&mpv_argv(
            &[
                PathBuf::from("/lib/clip.mov"),
                PathBuf::from("/lib/other.mp4"),
            ],
            1,
        ));
        assert_eq!(
            a,
            vec![
                "mpv",
                "--autofit-larger=100%x100%",
                "--playlist-start=1",
                "/lib/clip.mov",
                "/lib/other.mp4"
            ]
        );
    }

    #[test]
    fn ffplay_opens_one_file() {
        let a = os(&ffplay_argv(Path::new("/lib/clip.mov")));
        assert_eq!(a, vec!["ffplay", "/lib/clip.mov"]);
    }

    #[test]
    fn same_type_from_image_keeps_images() {
        let (pl, start) = same_type_playlist(&mixed(), 2);
        assert_eq!(
            pl,
            vec![PathBuf::from("/lib/a.jpg"), PathBuf::from("/lib/b.jpg")]
        );
        assert_eq!(start, 1);
    }

    #[test]
    fn same_type_from_video_keeps_videos() {
        let (pl, start) = same_type_playlist(&mixed(), 1);
        assert_eq!(
            pl,
            vec![
                PathBuf::from("/lib/clip.mov"),
                PathBuf::from("/lib/other.mp4")
            ]
        );
        assert_eq!(start, 0);
    }

    #[test]
    fn imv_preparation_leaves_non_heif_files_in_place() {
        let input = files();
        let (_temp, prepared) = prepare_heif_for_imv(&input).unwrap();
        assert_eq!(prepared, input);
    }
}
