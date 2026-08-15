//! Locating and driving the bundled ffmpeg binaries.
//!
//! ffmpeg handles what it is genuinely best at — demuxing, decoding every codec in existence, and
//! encoding the final master. It is never on the interactive path: playback reads our own frame
//! store instead (see `framestore`).

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Builds a Command that never flashes a console window on Windows.
pub fn command(exe: &Path) -> Command {
    let mut c = Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c.stdin(Stdio::null());
    c
}

fn candidates(name: &str) -> Vec<PathBuf> {
    let exe_name = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let mut v = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Installed layout: the binaries live in a sibling `ffmpeg` folder.
            v.push(dir.join("ffmpeg").join(&exe_name));
            v.push(dir.join(&exe_name));
            if let Some(up) = dir.parent() {
                v.push(up.join("ffmpeg").join(&exe_name));
            }
        }
    }
    v.push(PathBuf::from(&exe_name));
    v
}

fn locate(name: &str) -> Option<PathBuf> {
    for c in candidates(name) {
        if c.is_absolute() || c.components().count() > 1 {
            if c.is_file() {
                return Some(c);
            }
        } else if which(&c).is_some() {
            return which(&c);
        }
    }
    None
}

fn which(name: &Path) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    pub fn discover() -> Result<Self> {
        let ffmpeg = locate("ffmpeg")
            .ok_or_else(|| anyhow!("ffmpeg was not found next to the application or on PATH"))?;
        // Some minimal builds ship ffmpeg only; fall back to ffmpeg for probing in that case.
        let ffprobe = locate("ffprobe").unwrap_or_else(|| ffmpeg.clone());
        Ok(Self { ffmpeg, ffprobe })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProbeInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub has_video: bool,
    pub has_audio: bool,
}

/// Reads stream metadata with ffprobe's JSON output.
pub fn probe(tools: &Tools, path: &Path) -> Result<ProbeInfo> {
    let out = command(&tools.ffprobe)
        .args([
            "-v", "error",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("running ffprobe on {}", path.display()))?;

    if !out.status.success() {
        bail!(
            "ffprobe could not read this file: {}",
            String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("unknown error")
        );
    }

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("parsing ffprobe output")?;
    let mut info = ProbeInfo::default();

    if let Some(d) = v["format"]["duration"].as_str().and_then(|s| s.parse::<f64>().ok()) {
        info.duration = d;
    }

    if let Some(streams) = v["streams"].as_array() {
        for s in streams {
            match s["codec_type"].as_str() {
                Some("video") => {
                    // Cover art is tagged as a video stream; ignore it.
                    if s["disposition"]["attached_pic"].as_i64() == Some(1) {
                        continue;
                    }
                    if !info.has_video {
                        info.has_video = true;
                        info.width = s["width"].as_u64().unwrap_or(0) as u32;
                        info.height = s["height"].as_u64().unwrap_or(0) as u32;
                        info.fps = parse_rational(s["avg_frame_rate"].as_str().unwrap_or(""))
                            .or_else(|| parse_rational(s["r_frame_rate"].as_str().unwrap_or("")))
                            .unwrap_or(0.0);
                    }
                }
                Some("audio") => info.has_audio = true,
                _ => {}
            }
        }
    }

    if info.duration <= 0.0 {
        // Some containers only carry duration on the stream.
        if let Some(streams) = v["streams"].as_array() {
            for s in streams {
                if let Some(d) = s["duration"].as_str().and_then(|x| x.parse::<f64>().ok()) {
                    info.duration = info.duration.max(d);
                }
            }
        }
    }

    if !info.has_video && !info.has_audio {
        bail!("no video or audio streams found");
    }
    Ok(info)
}

fn parse_rational(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let n: f64 = n.parse().ok()?;
    let d: f64 = d.parse().ok()?;
    if d == 0.0 || n == 0.0 {
        return None;
    }
    Some(n / d)
}

/// Escapes a path for use inside an ffmpeg filtergraph argument, where `\`, `:` and `'` are all
/// significant. Windows paths hit every one of these.
pub fn escape_filter_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("\\\\\\'"),
            ':' => out.push_str("\\\\:"),
            '[' | ']' | ',' | ';' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Escapes text for ffmpeg's `drawtext` filter.
pub fn escape_drawtext(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\\\\\"),
            '\'' => out.push_str("\\\\\\'"),
            ':' => out.push_str("\\\\:"),
            '%' => out.push_str("\\\\%"),
            '\n' => out.push_str("\\n"),
            '[' | ']' | ',' | ';' | '=' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}
