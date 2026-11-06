//! 轻影 — one-click video compression that keeps the alpha channel.
//!
//! Drop a video (or pick one) and it re-encodes it with ffmpeg. Files that
//! carry an alpha channel are encoded as HEVC with alpha through Apple's
//! VideoToolbox (`-pix_fmt yuva420p -alpha_quality 1`), so the transparency
//! survives; everything else becomes H.264 in an MP4.

use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::Duration;

use nsh_base::theme;
use nsh_base::tooltip;
use nsh_core::platform::HttpRequest;
use nsh_core::prelude::*;

/// What a worker thread reports back to the engine thread.
///
/// Signals live in the engine thread's reactive graph, so the worker never
/// touches them: it sends these and the engine thread applies them.
enum Update {
    /// The dropped or picked inputs have been walked: the files they hold,
    /// for the user to confirm before anything is encoded.
    Scanned { files: Vec<PathBuf> },
    /// One file is starting, `done` of `total` already behind us.
    ItemStart {
        total: usize,
        done: usize,
        name: String,
    },
    /// How far the current file is, and which codec it took.
    ItemProgress { progress: f64, alpha: bool },
    /// The current file ended, well or badly.
    ItemDone { ok: bool, error: Option<String> },
    /// Every file was tried.
    BatchDone {
        total: usize,
        ok: usize,
        saved_from: u64,
        saved_to: u64,
        last_out: Option<PathBuf>,
        first_error: Option<String>,
        /// The user asked to stop: the tallies cover what finished, and the
        /// file in flight was abandoned.
        stopped: bool,
    },
    /// There was nothing to encode.
    Nothing { message: String },
}

/// What the app is currently doing.
///
/// There is only ever one kind of run: a batch of one or more files. A single
/// file is just a batch whose total is 1, so the UI and the worker have one
/// path to maintain instead of two.
#[derive(Clone, PartialEq)]
enum Job {
    Idle,
    /// Walking the input: which files it holds is not known yet.
    Scanning,
    /// The files that were dropped or picked, waiting for the user to start.
    /// Nothing is encoded until they say so, so this is where a file they
    /// changed their mind about is taken back out.
    Ready {
        files: Vec<PathBuf>,
    },
    Running {
        total: usize,
        /// Files already finished, this one excluded.
        done: usize,
        name: String,
        progress: f64,
        alpha: bool,
    },
    /// Every file was tried; the tallies say how it went.
    Finished {
        total: usize,
        ok: usize,
        saved_from: u64,
        saved_to: u64,
        /// The last output, for "show in Finder".
        last_out: Option<PathBuf>,
        /// The first failure, when any file failed.
        first_error: Option<String>,
        /// The user stopped it before every file was tried.
        stopped: bool,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Quality {
    /// q:v 55 — smaller file.
    Small,
    /// q:v 50 + full-quality alpha.
    Balanced,
    /// q:v 40 — closest to source.
    High,
}

impl Quality {
    fn label(self) -> &'static str {
        match self {
            Quality::Small => "小体积",
            Quality::Balanced => "均衡",
            Quality::High => "高画质",
        }
    }

    fn video_q(self) -> &'static str {
        match self {
            Quality::Small => "55",
            Quality::Balanced => "50",
            Quality::High => "40",
        }
    }

    fn alpha_quality(self) -> &'static str {
        match self {
            Quality::Small => "0.85",
            Quality::Balanced | Quality::High => "1",
        }
    }

    fn crf(self) -> &'static str {
        match self {
            Quality::Small => "23",
            Quality::Balanced => "18",
            Quality::High => "15",
        }
    }

    /// What the preset means, in the user's terms rather than ffmpeg's.
    ///
    /// The numbers alone ("q:v 50") say nothing to someone choosing a file
    /// size, so the hint leads with what changes and mentions the flag it
    /// maps to only as an aside.
    fn hint(self) -> &'static str {
        match self {
            Quality::Small => {
                "体积最小，适合发微信、传网盘。\n\
                 透明边缘可能有轻微毛边。\n\
                 无透明 CRF 23 / 带透明 q:v 55"
            }
            Quality::Balanced => {
                "默认档，肉眼基本看不出差别。\n\
                 透明通道保持完整。\n\
                 无透明 CRF 18 / 带透明 q:v 50"
            }
            Quality::High => {
                "最接近原片，适合还要再剪的素材。\n\
                 文件也最大。\n\
                 无透明 CRF 15 / 带透明 q:v 40"
            }
        }
    }
}

/// The app's UI set (`nsh.json` `"rust"`): what a TSX entry may import from
/// `video-squeezer/native`.
#[native_module("video-squeezer/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// Append a line to `~/qingying-debug.log`.
///
/// A packaged NSH app runs under NSH's own runner, which never calls this
/// crate's `main`, so `env_logger` is not initialised there and `log!` goes
/// nowhere. Writing the file directly is the only reliable trace.
fn trace(message: &str) {
    use std::io::Write;
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let path = PathBuf::from(home).join("qingying-debug.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{message}");
    }
}

fn path_from_uri(uri: &str) -> Option<PathBuf> {
    trace(&format!("[uri] raw = {uri:?}"));

    // Finder hands a drag over as a *file reference* URL — `file:///.file/
    // id=1234` — not a path. `Url::to_file_path` would happily turn that into
    // `/.file/id=1234`, which exists nowhere. AppKit resolves it properly, so
    // ask `NSURL` first and keep the plain parse as a fallback.
    #[cfg(target_os = "macos")]
    if let Some(path) = macos::resolve_file_url(uri) {
        trace(&format!("[uri] NSURL -> {}", path.display()));
        return Some(path);
    }

    match url::Url::parse(uri) {
        Ok(u) => {
            let path = u.to_file_path().ok();
            trace(&format!("[uri] url crate -> {path:?}"));
            path
        }
        Err(e) => {
            trace(&format!("[uri] parse failed: {e}"));
            None
        }
    }
}

/// AppKit's own URL handling, for what `url` cannot express.
#[cfg(target_os = "macos")]
mod macos {
    use std::path::PathBuf;

    use nsh_platform_macos::objc2_foundation::{NSString, NSURL};

    /// Turn a `file:` URL — reference form included — into a path.
    pub fn resolve_file_url(uri: &str) -> Option<PathBuf> {
        let text = NSString::from_str(uri);
        let url = NSURL::URLWithString(&text)?;
        // `filePathURL` resolves a file reference URL to its target; for a
        // plain file URL it is the URL itself.
        let file = url.filePathURL()?;
        file.path().map(|p| PathBuf::from(p.to_string()))
    }
}

fn fmt_bytes(bytes: u64) -> String {
    let mb = bytes as f64 / 1024.0 / 1024.0;
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else if mb >= 1.0 {
        format!("{:.1} MB", mb)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

/// Probe the source: duration in seconds, and whether it has an alpha channel.
fn probe(path: &PathBuf) -> Result<(f64, bool), String> {
    // Duration via ffprobe.
    let out = std::process::Command::new(find_tool("ffprobe"))
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .map_err(|e| format!("找不到 ffprobe：{e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("没有更多信息")
            .trim()
            .to_string();
        trace(&format!(
            "[probe] ffprobe failed for {}: {detail}",
            path.display()
        ));
        if !path.exists() {
            return Err("找不到这个文件，可能被移走了".into());
        }
        return Err(format!("ffprobe 读不出这个文件：{detail}"));
    }
    let duration: f64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0);
    if duration <= 0.0 {
        trace(&format!(
            "[probe] no duration for {}: {:?}",
            path.display(),
            String::from_utf8_lossy(&out.stdout)
        ));
        return Err("这个文件没有视频轨或时长为 0".into());
    }

    // Alpha: decode one frame as RGBA and look for non-opaque pixels.
    let mut child = std::process::Command::new(find_tool("ffmpeg"))
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("找不到 ffmpeg：{e}"))?;

    let mut frame = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let mut stdout = stdout;
        let _ = stdout.read_to_end(&mut frame);
    }
    let _ = child.wait();

    if frame.len() < 4 {
        return Err("解不出画面，可能不是视频".into());
    }
    // Sample every 97th pixel: enough to catch a transparent background.
    let sampled = frame.iter().skip(3).step_by(4 * 97);
    let mut total = 0usize;
    let mut clear = 0usize;
    for a in sampled {
        total += 1;
        if *a != 255 {
            clear += 1;
        }
    }
    let has_alpha = total > 0 && clear as f64 / total as f64 > 0.005;
    Ok((duration, has_alpha))
}

/// Path to write the result to, next to the source.
fn output_path(source: &PathBuf, alpha: bool) -> PathBuf {
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".into());
    let ext = if alpha { "mov" } else { "mp4" };
    source.with_file_name(format!("{stem}-squeezed.{ext}"))
}

/// How one file went.
struct Encoded {
    out: PathBuf,
    from: u64,
    to: u64,
}

/// The error `encode_one` returns when the user stopped the run.
///
/// Not a failure: the batch asked for it, so it is never shown as one.
const CANCELLED: &str = "__cancelled__";

/// The flag a running batch watches for the user's stop.
///
/// It lives in a signal so the button that sets it and the worker thread
/// that reads it share one flag. Comparing by pointer is what makes it a
/// signal value at all: the same flag is the same cancel, and reading it
/// must not look like a change.
#[derive(Clone)]
struct Cancel(Arc<AtomicBool>);

impl Cancel {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Ask the batch to stop after the file it is on.
    fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl PartialEq for Cancel {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Stream ffmpeg's stderr one record at a time, on either separator.
///
/// Live stats end in `\r` so a terminal overwrites the previous report in
/// place; diagnostics and the final report use `\n`. `BufRead::lines` only
/// splits on the latter, which held the whole encode in one buffer and left
/// the progress bar at 0 until ffmpeg exited.
fn stream_ffmpeg_output<R: Read>(reader: R, tx: Sender<String>) {
    let mut record = Vec::new();
    for byte in BufReader::new(reader).bytes().map_while(Result::ok) {
        match byte {
            b'\r' | b'\n' => {
                if record.is_empty() {
                    continue;
                }
                let text = String::from_utf8_lossy(&record).into_owned();
                record.clear();
                if tx.send(text).is_err() {
                    return;
                }
            }
            _ => record.push(byte),
        }
    }
    if !record.is_empty() {
        let _ = tx.send(String::from_utf8_lossy(&record).into_owned());
    }
}

/// `frame=... time=00:00:07.20 ...` → how far into `duration` that is.
///
/// The first report ffmpeg emits has `time=N/A`; there is no fraction to show
/// until a real timestamp arrives.
fn progress_percent(line: &str, duration: f64) -> Option<f64> {
    let (_, rest) = line.split_once("time=")?;
    let stamp = rest.split_whitespace().next()?;
    let seconds = parse_timestamp(stamp)?;
    Some((seconds / duration).clamp(0.0, 1.0))
}

/// Encode `source`, reporting progress through `on_progress`.
///
/// Returns the output's size and path, or a message to show the user.
fn encode_one(
    source: &PathBuf,
    quality: Quality,
    cancel: &AtomicBool,
    on_progress: &dyn Fn(f64, bool),
) -> Result<Encoded, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    let (duration, has_alpha) = probe(source)?;
    // Probing decodes a frame; a stop during it must not start an encode.
    if cancel.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }

    let out_path = output_path(source, has_alpha);
    let from_size = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);
    on_progress(0.0, has_alpha);

    let mut cmd = std::process::Command::new(find_tool("ffmpeg"));
    // ffmpeg looks up its own helpers relative to PATH; give it the usual
    // directories rather than the stub PATH a Finder launch provides.
    cmd.env("PATH", EXTRA_BIN_DIRS.join(":"));
    cmd.arg("-y").arg("-i").arg(&source);

    if has_alpha {
        cmd.args([
            "-c:v",
            "hevc_videotoolbox",
            "-pix_fmt",
            "yuva420p",
            "-alpha_quality",
            quality.alpha_quality(),
            "-tag:v",
            "hvc1",
            "-q:v",
            quality.video_q(),
            "-c:a",
            "aac",
            "-b:a",
            "128k",
        ]);
    } else {
        cmd.args([
            "-c:v",
            "libx264",
            "-crf",
            quality.crf(),
            "-preset",
            "medium",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-movflags",
            "+faststart",
        ]);
    }
    cmd.arg(&out_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg 启动失败：{e}"))?;

    // ffmpeg reports progress on stderr as `... time=00:00:07.20 ...`, with
    // each live report separated by `\r` rather than `\n`.
    let Some(stderr) = child.stderr.take() else {
        let _ = child.wait();
        return Err("读不到 ffmpeg 的进度输出".into());
    };
    // ffmpeg's stderr is read on its own thread so this one can keep looking
    // at the cancel flag. Reading it inline would block on `read` until the
    // next record arrives, and a stalled ffmpeg would then ignore the user's
    // stop for as long as it stalled.
    let (lines_tx, lines_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || stream_ffmpeg_output(stderr, lines_tx));

    let mut last_error = String::new();
    let mut stopped = false;
    loop {
        match lines_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                if let Some(pct) = progress_percent(&line, duration) {
                    on_progress(pct, has_alpha);
                } else if !line.contains("time=") && !line.trim().is_empty() {
                    last_error = line;
                }
            }
            // Nothing said for 100 ms: the only reason to act is the user.
            Err(RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Relaxed) {
                    stopped = true;
                    let _ = child.kill();
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = child.wait();

    // A killed ffmpeg leaves the half-written output behind; it is not a
    // result anyone asked for, so it goes.
    if stopped {
        let _ = std::fs::remove_file(&out_path);
        return Err(CANCELLED.into());
    }

    if !out_path.exists() {
        return Err(if last_error.is_empty() {
            "ffmpeg 没有产出文件，编码失败".into()
        } else {
            last_error
        });
    }
    let to_size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    on_progress(1.0, has_alpha);
    Ok(Encoded {
        out: out_path,
        from: from_size,
        to: to_size,
    })
}

/// Extensions this app will try to encode.
const VIDEO_EXTS: &[&str] = &[
    "mov", "mp4", "m4v", "avi", "mkv", "webm", "mpg", "mpeg", "wmv", "flv", "ts", "mts", "m2ts",
];

fn is_video(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            VIDEO_EXTS.contains(&lower.as_str())
        })
        .unwrap_or(false)
}

/// Turn what was dropped or picked into the list of files to encode.
///
/// A dropped folder is walked one level deep — not recursively — so a folder
/// of clips behaves as expected without silently pulling in a whole archive.
/// Files this app already produced (`-squeezed`) are skipped, so a second run
/// over the same folder does not encode its own output.
fn collect_inputs(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let Ok(entries) = std::fs::read_dir(input) else {
                trace(&format!("[scan] cannot read {}", input.display()));
                continue;
            };
            let mut found: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file() && is_video(path))
                .collect();
            found.sort();
            trace(&format!(
                "[scan] {} -> {} videos",
                input.display(),
                found.len()
            ));
            files.extend(found);
        } else if is_video(input) {
            files.push(input.clone());
        } else {
            trace(&format!("[scan] skipped {}", input.display()));
        }
    }
    files.retain(|path| {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string());
        !name.unwrap_or_default().contains("-squeezed")
    });
    // A folder and one of its files can both be dropped; encode each once.
    let mut seen = std::collections::HashSet::new();
    files.retain(|path| seen.insert(path.clone()));
    files
}

/// Walk `inputs` and report the files they hold.
///
/// Runs on a worker thread: a dropped folder can hold thousands of entries,
/// and the engine thread must keep drawing while it is read.
fn scan_batch(inputs: Vec<PathBuf>, tx: Sender<Update>) {
    let files = collect_inputs(&inputs);
    if files.is_empty() {
        let _ = tx.send(Update::Nothing {
            message: "这里没有可压缩的视频".into(),
        });
        return;
    }
    let _ = tx.send(Update::Scanned { files });
}

/// Encode `files`, one after another, reporting to `tx`.
///
/// Runs on a worker thread; the engine thread drains `tx` with `pump`.
/// `cancel` is the user's stop: the file in flight is killed and the rest
/// are left alone.
fn encode_batch(files: Vec<PathBuf>, quality: Quality, cancel: Cancel, tx: Sender<Update>) {
    let total = files.len();
    let mut ok = 0usize;
    let mut saved_from = 0u64;
    let mut saved_to = 0u64;
    let mut last_out: Option<PathBuf> = None;
    let mut first_error: Option<String> = None;

    // Between files, before each one: a stop the last file honoured leaves
    // the rest untouched rather than starting one more.
    let mut stopped = false;
    for (index, file) in files.iter().enumerate() {
        if cancel.stopped() {
            stopped = true;
            break;
        }
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let _ = tx.send(Update::ItemStart {
            total,
            done: index,
            name: name.clone(),
        });

        let sender = tx.clone();
        let progress = move |pct: f64, alpha: bool| {
            let _ = sender.send(Update::ItemProgress {
                progress: pct,
                alpha,
            });
        };

        match encode_one(file, quality, &cancel.0, &progress) {
            Ok(done) => {
                ok += 1;
                saved_from += done.from;
                saved_to += done.to;
                last_out = Some(done.out);
                let _ = tx.send(Update::ItemDone {
                    ok: true,
                    error: None,
                });
            }
            // The user's stop, not a failure: no error to report, and the
            // batch ends here.
            Err(message) if message == CANCELLED => {
                stopped = true;
                break;
            }
            Err(message) => {
                trace(&format!("[encode] {name} failed: {message}"));
                if first_error.is_none() {
                    first_error = Some(format!("{name}：{message}"));
                }
                let _ = tx.send(Update::ItemDone {
                    ok: false,
                    error: Some(message),
                });
            }
        }
    }

    let _ = tx.send(Update::BatchDone {
        total,
        ok,
        saved_from,
        saved_to,
        last_out,
        first_error,
        stopped,
    });
}

/// This app's version, as the bundle records it.
///
/// `nsh package` writes the version into `Contents/Resources/nsh/app.json`
/// from `nsh.json`, overridden by `--app-version` — which is what the release
/// workflow passes. Reading it at run time keeps the updater's idea of "what
/// I am" in step with what Finder shows, instead of a constant that has to be
/// bumped by hand.
fn current_version() -> String {
    const FALLBACK: &str = env!("CARGO_PKG_VERSION");

    let Ok(exe) = std::env::current_exe() else {
        return FALLBACK.to_string();
    };
    // Contents/MacOS/<binary> -> Contents/Resources/nsh/app.json
    let Some(contents) = exe.parent().and_then(|p| p.parent()) else {
        return FALLBACK.to_string();
    };
    let manifest = contents.join("Resources/nsh/app.json");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return FALLBACK.to_string();
    };
    // A one-key lookup, so no JSON parser: find `"version"` and read the
    // string that follows it.
    text.split("\"version\"")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
        .map(str::to_owned)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| FALLBACK.to_string())
}

/// Where releases come from.
///
/// The Atom feed rather than `api.github.com`: the REST API allows 60
/// unauthenticated requests an hour per address, which a shared or corporate
/// address can exhaust and every check would then fail. The feed is plain
/// XML, served to anyone, with no limits to hit.
const RELEASES_FEED: &str = "https://github.com/richardwild426/qingying/releases.atom";
const RELEASES_PAGE: &str = "https://github.com/richardwild426/qingying/releases/latest";

/// What the update check found.
#[derive(Clone, PartialEq, Debug)]
enum UpdateCheck {
    /// Nothing asked yet.
    Idle,
    Checking,
    UpToDate {
        current: String,
    },
    Available {
        current: String,
        latest: String,
        url: String,
        /// The release asset this platform installs.
        asset: String,
    },
    /// The DMG is downloading; `received` of `total` bytes, 0 if unknown.
    Downloading {
        latest: String,
        received: u64,
        total: u64,
    },
    /// Downloaded and installed: the new version is on disk, waiting for a
    /// relaunch.
    Installed {
        latest: String,
    },
    Failed {
        message: String,
    },
}

/// Split `1.2.3` into comparable numbers, ignoring a leading `v`.
///
/// A tag like `v1.2.3-beta` compares as `1.2.3`: a pre-release is this app's
/// own convention for something older than the release it precedes.
fn version_parts(tag: &str) -> Vec<u64> {
    tag.trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .collect()
}

/// Whether `latest` is newer than `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    let (a, b) = (version_parts(latest), version_parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// The release asset this platform installs, from a tag and the app's name.
///
/// The name is the one the workflow uploads, built from the same rule, so a
/// download needs no API call and no extra metadata.
const ASSET_NAME: &str = "QingYing-macos-arm64.dmg";

/// Where the DMG for `tag` lives. GitHub serves it from a fixed path, so the
/// URL can be built without asking the API first.
fn asset_url(tag: &str) -> String {
    format!("https://github.com/richardwild426/qingying/releases/download/{tag}/{ASSET_NAME}")
}

/// This app's bundle name, e.g. `轻影.app`.
///
/// Taken from the running executable's path rather than a constant: the bundle
/// is what gets replaced, and the image holds a copy with the same name.
fn bundle_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.ancestors()
                .find(|p| p.extension().is_some_and(|e| e == "app"))
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "轻影.app".to_string())
}

/// Where the downloaded image is written before mounting.
fn temp_dmg_path() -> Option<PathBuf> {
    let dir = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Some(dir.join("qingying-update.dmg"))
}

/// What a download reports back to the engine thread.
enum DownloadEvent {
    /// Bytes on disk so far, and the size the server announced (0 unknown).
    Progress {
        received: u64,
        total: u64,
    },
    Installed,
    Failed(String),
}

/// Download `url` into `dest`, reporting what arrived.
///
/// Through `fetch`, not a spawned `curl`: a subprocess reaches the network
/// behind the app's `capabilities` (which grant `github.com` and
/// `release-assets.githubusercontent.com` and nothing else), and a test's
/// stubbed network cannot see it at all — a download that never happened and
/// one that bypassed the sandbox look the same from outside.
///
/// `fetch` hands back the whole body, so the report is one final count, not
/// a running one. The UI says "下载中" until then rather than showing a
/// number that cannot move.
async fn download_to(url: &str, dest: &PathBuf) -> Result<u64, String> {
    let response = fetch(HttpRequest::get(url))
        .await
        .map_err(|message| format!("下载失败：{message}"))?;
    if !response.ok() {
        return Err(format!("下载失败：HTTP {}", response.status()));
    }
    let bytes = response.body;
    if bytes.is_empty() {
        return Err("下载失败：文件是空的".into());
    }
    std::fs::write(dest, &bytes).map_err(|e| format!("写入镜像失败：{e}"))?;
    Ok(bytes.len() as u64)
}

/// Mount the image, copy the app over the running one, and unmount.
///
/// Runs on a worker thread: it shells out to `hdiutil`, which blocks. The
/// bundle is replaced in place, which works while it runs — macOS keeps the
/// old executable mapped until the process exits, so the new files land on
/// disk and take effect on the next launch.
fn install_from_dmg(dmg: &PathBuf, app_name: &str) -> Result<(), String> {
    let mount = "/Volumes/QingYingUpdate";
    let _ = std::process::Command::new("/usr/bin/hdiutil")
        .args(["detach", mount, "-force"])
        .output();

    let attach = std::process::Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-mountpoint", mount])
        .arg(dmg)
        .output()
        .map_err(|e| format!("挂载失败：{e}"))?;
    if !attach.status.success() {
        return Err(format!(
            "挂载失败：{}",
            String::from_utf8_lossy(&attach.stderr).trim()
        ));
    }

    // Everything watched below happens while the image is mounted, so the
    // failure path must unmount too.
    let source = PathBuf::from(mount).join(app_name);
    let result = (|| -> Result<(), String> {
        if !source.is_dir() {
            return Err(format!("镜像里没有 {app_name}"));
        }

        // Where this app lives now: /Applications/轻影.app.
        let exe = std::env::current_exe().map_err(|e| format!("找不到自身位置：{e}"))?;
        let target = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .ok_or("自身位置不对")?
            .to_path_buf();
        trace(&format!(
            "[update] {} -> {}",
            source.display(),
            target.display()
        ));

        // Replace the bundle: a copy over a live app can leave a half-written
        // one if it fails, so the old one is moved aside first and restored
        // when the copy does not land.
        if !target.is_dir() {
            return Err(format!("当前应用不在 {}，无法就地替换", target.display()));
        }
        let backup = target.with_extension("app.old");
        let _ = std::fs::remove_dir_all(&backup);
        std::fs::rename(&target, &backup).map_err(|e| format!("无法移开旧版本：{e}"))?;

        let copied = std::process::Command::new("/usr/bin/ditto")
            .arg(&source)
            .arg(&target)
            .output();
        match copied {
            Ok(out) if out.status.success() => {
                let _ = std::fs::remove_dir_all(&backup);
                Ok(())
            }
            Ok(out) => {
                let _ = std::fs::remove_dir_all(&target);
                let _ = std::fs::rename(&backup, &target);
                Err(format!(
                    "复制新版本失败：{}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&target);
                let _ = std::fs::rename(&backup, &target);
                Err(format!("复制新版本失败：{e}"))
            }
        }
    })();

    let _ = std::process::Command::new("/usr/bin/hdiutil")
        .args(["detach", mount, "-force"])
        .output();

    result
}

/// Read the newest release's tag from the Atom feed.
///
/// The feed's first entry is the most recent release; its `<title>` is the
/// tag (`v1.0.0`) and its `<link href="…">` the page to open. Parsed by hand
/// rather than with an XML crate: this is the only XML the app reads.
fn parse_latest_release(body: &str) -> Result<(String, String), String> {
    let entries = body.split("<entry>").nth(1).ok_or("发布源里没有版本记录")?;

    let tag = entries
        .split("<title>")
        .nth(1)
        .and_then(|rest| rest.split("</title>").next())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or("发布源里没有版本号")?;

    // The entry's link is the release page; fall back to the latest alias.
    let url = entries
        .split("href=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .filter(|u| u.starts_with("http"))
        .map(str::to_owned)
        .unwrap_or_else(|| RELEASES_PAGE.to_string());

    Ok((tag, url))
}

/// Where Homebrew puts the binaries a GUI app cannot find.
///
/// An app opened from Finder inherits launchd's PATH — `/usr/bin:/bin:
/// /usr/sbin:/sbin` — so `/opt/homebrew/bin` is not on it and `ffmpeg` is
/// invisible, even though it works in a terminal. Every lookup tries these.
const EXTRA_BIN_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/opt/local/bin",
    "/usr/bin",
    "/bin",
];

/// Locate a binary, falling back to the usual Homebrew locations.
fn find_tool(name: &str) -> PathBuf {
    for dir in EXTRA_BIN_DIRS {
        let candidate = PathBuf::from(dir).join(name);
        if candidate.is_file() {
            trace(&format!("[tool] {name} -> {}", candidate.display()));
            return candidate;
        }
    }
    trace(&format!(
        "[tool] {name} NOT FOUND in {EXTRA_BIN_DIRS:?}; PATH={:?}",
        std::env::var("PATH")
    ));
    // Nothing on disk: let the OS try PATH, and let the spawn error say so.
    PathBuf::from(name)
}

/// Shrink the window to the size this tool actually needs.
///
/// `nsh package`'s runner opens every app at 1200x800, and nothing in
/// `nsh.json` changes that — a single form would float in a sea of white.
/// The window is resized once, on the main queue, by AppKit itself.
#[cfg(target_os = "macos")]
fn fit_window(width: f64, height: f64) {
    use nsh_platform_macos::objc2::MainThreadMarker;
    use nsh_platform_macos::objc2_app_kit::NSApplication;
    use nsh_platform_macos::objc2_foundation::{NSPoint, NSRect, NSSize};

    std::thread::spawn(move || {
        // The window does not exist during the first frame's layout, so this
        // retries on the main queue until one is there and can be resized.
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let (tx, rx) = std::sync::mpsc::channel();
            dispatch2::DispatchQueue::main().exec_async(move || {
                let done = (|| {
                    let mtm = MainThreadMarker::new()?;
                    let app = NSApplication::sharedApplication(mtm);
                    // Not necessarily `mainWindow`: take the first real window
                    // with a title, which is the app's own.
                    let windows = app.windows();
                    let window = (0..windows.count())
                        .map(|i| windows.objectAtIndex(i))
                        .find(|w| w.isVisible() && w.contentView().is_some())?;
                    let screen = window.screen()?;
                    let visible = screen.visibleFrame();
                    let size = NSSize::new(width, height);
                    let origin = NSPoint::new(
                        visible.origin.x + (visible.size.width - width) / 2.0,
                        visible.origin.y + (visible.size.height - height) / 2.0,
                    );
                    window.setContentSize(size);
                    window.setFrame_display(NSRect::new(origin, size), true);
                    window.center();
                    Some(())
                })()
                .is_some();
                let _ = tx.send(done);
            });
            if rx.recv().unwrap_or(false) {
                break;
            }
        }
    });
}

/// Show the file in Finder, selected.
fn reveal(path: &PathBuf) {
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .status();
}

/// Open the file in whatever app owns it.
fn open_path(path: &PathBuf) {
    let _ = std::process::Command::new("open").arg(path).status();
}

/// Apply worker updates to the UI signals, on the engine thread.
///
/// The worker runs one file at a time and reports each step; this folds those
/// steps into the single `Job` the UI draws.
fn pump(
    rx: Receiver<Update>,
    job: Signal<Job>,
    progress: Signal<f64>,
    carry: Signal<Vec<PathBuf>>,
    pending: Signal<Vec<PathBuf>>,
    owner: Owner,
) {
    let rx = std::rc::Rc::new(std::cell::RefCell::new(rx));

    // Everything here — the liveness flag and the timer that reads it — is
    // made under the app's owner, not the caller's. `pump` is called from a
    // press handler, and whatever a handler makes belongs to the region it
    // ran in: a timer and flag started while the waiting list was showing
    // died with that list, and the run's progress then never reached the
    // window.
    owner.run(move || {
        let live = signal(true);
        set_interval(Duration::from_millis(80), move || {
            if !live.get() {
                return;
            }
            let mut finished = false;
            loop {
                let Ok(guard) = rx.try_borrow() else {
                    break;
                };
                let event = guard.try_recv();
                drop(guard);

                match event {
                    // The inputs are known but nothing has been encoded: this is
                    // the moment the user confirms the list or trims it.
                    Ok(Update::Scanned { files }) => {
                        progress.set(0.0);
                        // A second drop adds to a list that is already waiting,
                        // each file once.
                        let mut all = carry.get();
                        for file in files {
                            if !all.contains(&file) {
                                all.push(file);
                            }
                        }
                        carry.set(Vec::new());
                        pending.set(all.clone());
                        job.set(Job::Ready { files: all });
                        finished = true;
                    }
                    Ok(Update::ItemStart { total, done, name }) => {
                        progress.set(0.0);
                        job.set(Job::Running {
                            total,
                            done,
                            name,
                            progress: 0.0,
                            alpha: false,
                        });
                    }
                    Ok(Update::ItemProgress {
                        progress: pct,
                        alpha,
                    }) => {
                        progress.set(pct);
                        if let Job::Running {
                            total,
                            done,
                            name,
                            progress: _,
                            alpha: was_alpha,
                        } = job.get()
                        {
                            job.set(Job::Running {
                                total,
                                done,
                                name,
                                progress: pct,
                                alpha: alpha || was_alpha,
                            });
                        }
                    }
                    Ok(Update::ItemDone { ok, error }) => {
                        if !ok {
                            if let Some(message) = error {
                                trace(&format!("[pump] item failed: {message}"));
                            }
                        }
                        // Keep the tallies on the running state so a failure
                        // midway still leaves the numbers readable.
                        if let Job::Running {
                            total,
                            done,
                            name,
                            alpha,
                            ..
                        } = job.get()
                        {
                            job.set(Job::Running {
                                total,
                                done: done + 1,
                                name,
                                progress: if ok { 1.0 } else { 0.0 },
                                alpha,
                            });
                        }
                    }
                    Ok(Update::BatchDone {
                        total,
                        ok,
                        saved_from,
                        saved_to,
                        last_out,
                        first_error,
                        stopped,
                    }) => {
                        job.set(Job::Finished {
                            total,
                            ok,
                            saved_from,
                            saved_to,
                            last_out,
                            first_error,
                            stopped,
                        });
                        finished = true;
                    }
                    Ok(Update::Nothing { message }) => {
                        job.set(Job::Failed { message });
                        finished = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
                if finished {
                    break;
                }
            }
            if finished {
                live.set(false);
            }
        });
    });
}

/// Drive a future to completion on the calling thread.
///
/// The dialogs are the only futures this app has, and they resolve on the main
/// thread's run loop, so a full executor would be dead weight here.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// `00:00:07.20` → 7.2
fn parse_timestamp(stamp: &str) -> Option<f64> {
    let mut parts = stamp.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

/// A rounded, themed button.
fn button(
    label: impl Into<SharedString>,
    primary: bool,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let label = label.into();
    view()
        .px_4()
        .py_2()
        .rounded(px(8.0))
        .bg(move || {
            if primary {
                theme().primary
            } else {
                theme().muted
            }
        })
        .active(|s| s.opacity(0.7))
        .on_press(on_press)
        .child(text(label).text_size(px(13.0)).text_color(move || {
            if primary {
                theme().primary_foreground
            } else {
                theme().foreground
            }
        }))
}

/// An icon a button may show, drawn with a glyph so the app stays dependency-free.
#[derive(Clone, Copy, PartialEq)]
enum Icon {
    /// "Show in Finder": a magnifier over a folder, as macOS uses.
    Reveal,
    /// "Open": a play triangle.
    Play,
}

impl Icon {
    fn glyph(self) -> &'static str {
        match self {
            // Both are in every macOS system font, unlike the symbol-only
            // codepoints that would fall back to a tofu box.
            Icon::Reveal => "⌕",
            Icon::Play => "▶",
        }
    }
}

/// A button with a leading icon, sized to a comfortable hit area.
///
/// A leading glyph makes the action readable at a glance, which is how
/// macOS's own result views pair "Show in Finder" with a folder symbol.
fn button_with_icon(
    label: impl Into<SharedString>,
    icon: Icon,
    primary: bool,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let label = label.into();
    view()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .h(px(28.0))
        .px_4()
        .rounded(px(7.0))
        .bg(move || {
            if primary {
                theme().primary
            } else {
                theme().background
            }
        })
        .border(px(1.0))
        .border_color(move || {
            if primary {
                theme().primary
            } else {
                theme().border
            }
        })
        .active(|s| s.opacity(0.65))
        .on_press(on_press)
        .child(text(icon.glyph()).text_size(px(10.0)).text_color(move || {
            if primary {
                theme().primary_foreground
            } else {
                theme().muted_foreground
            }
        }))
        .child(text(label).text_size(px(12.0)).text_color(move || {
            if primary {
                theme().primary_foreground
            } else {
                theme().foreground
            }
        }))
}

/// The application's root component.
#[component]
pub fn App() -> impl IntoElement {
    // The runner's default 1200x800 is far more than this tool needs.
    #[cfg(target_os = "macos")]
    fit_window(560.0, 620.0);

    let job = signal(Job::Idle);
    let progress = signal(0.0_f64);
    let hovering = signal(false);
    let quality = signal(Quality::Balanced);

    let current_version = current_version();
    let update = signal(UpdateCheck::Idle);

    // Checking is manual, and one check runs at a time: `fetch` is awaited in
    // a task the engine owns, so the signals it writes stay on the engine
    // thread.
    // The owner of the app's own layout, captured once, before anything can
    // press a button. What a press handler creates belongs to the region it
    // ran in, and is disposed with it — a timer, a spawned task, even a
    // signal. Both the batch pump and the update download outlive the
    // button that started them, so they are made under this owner instead.
    let app_owner = Owner::current();

    let check_now = {
        let current = current_version.clone();
        move || {
            if matches!(update.get(), UpdateCheck::Checking) {
                return;
            }
            trace("[update] check pressed");
            update.set(UpdateCheck::Checking);
            let current = current.clone();
            // Under the app's owner: `update.set` above swaps the button that
            // was pressed out of the window, and a task made in its region
            // would be cancelled before it could finish.
            app_owner.run(move || {
                spawn(async move {
                    let request = HttpRequest::get(RELEASES_FEED);
                    match fetch(request).await {
                        Ok(response) if response.ok() => {
                            match parse_latest_release(&response.text()) {
                                Ok((tag, url)) => {
                                    if is_newer(&tag, &current) {
                                        update.set(UpdateCheck::Available {
                                            current,
                                            latest: tag,
                                            url,
                                            asset: ASSET_NAME.to_string(),
                                        });
                                    } else {
                                        update.set(UpdateCheck::UpToDate { current });
                                    }
                                }
                                Err(message) => update.set(UpdateCheck::Failed { message }),
                            }
                        }
                        Ok(response) => update.set(UpdateCheck::Failed {
                            message: format!("GitHub 返回 HTTP {}", response.status()),
                        }),
                        Err(message) => update.set(UpdateCheck::Failed { message }),
                    }
                })
            });
        }
    };

    // Download the release's DMG and swap the running bundle for it.
    //
    // `fetch` must be awaited on the engine thread's runtime, and mounting
    // shells out to `hdiutil`, which blocks. So: fetch in a task, then hand
    // the bytes to a worker for writing and installing, and let the engine
    // thread apply what it reports.
    let install_now = {
        move || {
            trace(&format!("[update] install_now; state={:?}", update.get()));
            let UpdateCheck::Available { latest, .. } = update.get() else {
                return;
            };
            if matches!(
                update.get(),
                UpdateCheck::Downloading { .. } | UpdateCheck::Installed { .. }
            ) {
                return;
            }
            update.set(UpdateCheck::Downloading {
                latest: latest.clone(),
                received: 0,
                total: 0,
            });

            let name = bundle_name();
            // Under the app's owner, for the same reason as `check_now`: the
            // `Downloading` state above replaces the button that was pressed,
            // so a task made in that button's region would be cancelled
            // immediately — the download then never starts, and the window
            // sits at 0% with nothing in the log.
            app_owner.run(move || {
                // `fetch` is awaited on the engine thread's runtime, and
                // mounting shells out to `hdiutil`, which blocks: so the
                // download is a task here, and the bytes go to a worker to be
                // written and installed.
                let (tx, rx) = std::sync::mpsc::channel::<DownloadEvent>();
                let task_latest = latest.clone();
                spawn(async move {
                    let url = asset_url(&task_latest);
                    trace(&format!("[update] downloading {url}"));
                    let Some(dmg) = temp_dmg_path() else {
                        update.set(UpdateCheck::Failed {
                            message: "找不到临时目录".into(),
                        });
                        return;
                    };
                    let bytes = match download_to(&url, &dmg).await {
                        Ok(bytes) => bytes,
                        Err(message) => {
                            trace(&format!("[update] {message}"));
                            update.set(UpdateCheck::Failed { message });
                            return;
                        }
                    };
                    trace(&format!("[update] {bytes} bytes on disk, installing"));
                    let _ = tx.send(DownloadEvent::Progress {
                        received: bytes,
                        total: bytes,
                    });
                    std::thread::spawn(move || {
                        let event = match install_from_dmg(&dmg, &name) {
                            Ok(()) => DownloadEvent::Installed,
                            Err(message) => DownloadEvent::Failed(message),
                        };
                        let _ = std::fs::remove_file(&dmg);
                        let _ = tx.send(event);
                    });
                });

                // Poll the worker on the engine thread, as the picker does.
                let finished = std::rc::Rc::new(std::cell::RefCell::new(Some(rx)));
                let done = signal(false);
                set_interval(Duration::from_millis(120), move || {
                    if done.get() {
                        return;
                    }
                    let Some(rx) = finished.borrow_mut().take() else {
                        return;
                    };
                    // Drain what arrived, keeping the last progress so a burst
                    // of them does not redraw once per event.
                    let mut last_progress = None;
                    let mut outcome: Option<Option<String>> = None;
                    loop {
                        match rx.try_recv() {
                            Ok(DownloadEvent::Progress { received, total }) => {
                                last_progress = Some((received, total));
                            }
                            Ok(DownloadEvent::Installed) => {
                                outcome = Some(None);
                                break;
                            }
                            Ok(DownloadEvent::Failed(message)) => {
                                outcome = Some(Some(message));
                                break;
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => break,
                        }
                    }
                    if let Some((received, total)) = last_progress {
                        update.set(UpdateCheck::Downloading {
                            latest: latest.clone(),
                            received,
                            total,
                        });
                    }
                    match outcome {
                        Some(None) => {
                            done.set(true);
                            update.set(UpdateCheck::Installed {
                                latest: latest.clone(),
                            });
                        }
                        Some(Some(message)) => {
                            done.set(true);
                            update.set(UpdateCheck::Failed { message });
                        }
                        None => *finished.borrow_mut() = Some(rx),
                    }
                });
            });
        }
    };

    // The stop flag of the run in progress, if any. A signal so the button
    // that sets it and the worker that reads it share one flag, and so that
    // `start` stays `Copy` (a closure capturing an `Rc` would not be, and it
    let cancel = signal(None::<Cancel>);
    // What a scan is adding to: the files already waiting, which it adds to
    // rather than replaces.
    let carry = signal(Vec::<PathBuf>::new());
    // The waiting list itself, on its own so the rows read one plain signal.
    // `Job::Ready` mirrors it for the states that only ask "is anything
    // waiting"; `pending` is what the list draws.
    let pending = signal(Vec::<PathBuf>::new());

    // Signals belong to the engine thread's reactive graph, so worker threads
    // never write them: they send `Update`s and the engine thread applies them
    // from a timer. This is what makes the progress bar and the picker safe.
    //
    // Dropping or picking only *collects*: the files land in `Job::Ready` and
    // wait. Encoding starts when the user says so.
    let start = move |inputs: Vec<PathBuf>| {
        for path in &inputs {
            trace(&format!(
                "[start] {} exists={} is_dir={} is_file={}",
                path.display(),
                path.exists(),
                path.is_dir(),
                path.is_file()
            ));
        }
        // A waiting list takes more files (dropping a second folder adds to
        // it); a run in progress does not, since its total is already fixed.
        if matches!(job.get(), Job::Running { .. } | Job::Scanning { .. }) {
            trace("[start] ignored: a batch is already running");
            return;
        }
        // Files already waiting stay in the list: this scan adds to them.
        carry.set(pending.get());
        progress.set(0.0);
        job.set(Job::Scanning);
        cancel.set(None);
        let (tx, rx) = std::sync::mpsc::channel::<Update>();
        std::thread::spawn(move || scan_batch(inputs, tx));
        pump(rx, job, progress, carry, pending, app_owner);
    };

    // Turn the waiting list into a run.
    let begin = move || {
        let files = pending.get();
        if files.is_empty() {
            return;
        }
        let flag = Cancel::new();
        cancel.set(Some(flag.clone()));
        progress.set(0.0);
        job.set(Job::Running {
            total: files.len(),
            done: 0,
            name: String::new(),
            progress: 0.0,
            alpha: false,
        });
        let (tx, rx) = std::sync::mpsc::channel::<Update>();
        let settings = quality.get();
        // The list has become the run: nothing is left waiting.
        pending.set(Vec::new());
        carry.set(Vec::new());
        std::thread::spawn(move || encode_batch(files, settings, flag, tx));
        pump(rx, job, progress, carry, pending, app_owner);
    };

    // Ask the run in progress to stop. The worker kills ffmpeg and ends the
    // batch; the UI keeps showing progress until it reports back.
    let stop = move || {
        if let Some(flag) = cancel.get() {
            flag.stop();
            trace("[stop] asked the batch to stop");
        }
    };

    // Take one file back out of the waiting list.
    let remove = move |path: PathBuf| {
        let kept: Vec<PathBuf> = pending.get().into_iter().filter(|p| *p != path).collect();
        pending.set(kept.clone());
        // The last one gone: there is nothing left to start, so the window
        // goes back to its empty state instead of showing an empty list.
        job.set(if kept.is_empty() {
            Job::Idle
        } else {
            Job::Ready { files: kept }
        });
    };

    let clear = move || {
        pending.set(Vec::new());
        job.set(Job::Idle);
    };

    // NSH runs press handlers on its engine thread, but AppKit builds an open
    // panel only on the main thread: `FileDialog::pick_file` blocks and traps
    // there. `AsyncFileDialog` hops to the main thread itself, so it runs on a
    // worker and reports the choice over a channel.
    let browse = move || {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        std::thread::spawn(move || {
            let chosen = block_on(
                rfd::AsyncFileDialog::new()
                    .add_filter("视频", &VIDEO_EXTS.to_vec())
                    .pick_files(),
            );
            let paths = chosen
                .map(|handles| {
                    handles
                        .iter()
                        .map(|handle| handle.path().to_path_buf())
                        .collect()
                })
                .unwrap_or_default();
            let _ = tx.send(paths);
        });

        // Poll on the engine thread; the panel keeps the main thread's run loop.
        let answer = std::rc::Rc::new(std::cell::RefCell::new(rx));
        let waiting = signal(true);
        set_interval(Duration::from_millis(100), move || {
            if !waiting.get() {
                return;
            }
            let Ok(guard) = answer.try_borrow() else {
                return;
            };
            match guard.try_recv() {
                Ok(paths) => {
                    waiting.set(false);
                    drop(guard);
                    if !paths.is_empty() {
                        start(paths);
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => waiting.set(false),
            }
        });
    };

    // The waiting list's rows, made here rather than inside the branch that
    // shows them: a region is rebuilt every time its condition flips, and an
    // `each` rebuilt that way stops following the signal it lists — the rows
    // would then keep files that were taken out. Made once, it follows
    // `pending` for the life of the window.
    let list = view().gap_1().child(each(
        pending,
        |path| path.clone(),
        move |path: ReadSignal<PathBuf>, _| {
            let path = path.get();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded(px(6.0))
                .bg(move || theme().muted)
                .child(
                    text(name)
                        .text_size(px(12.0))
                        .text_color(move || theme().foreground),
                )
                .child(view().flex_1())
                .child(
                    view()
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .active(|s| s.opacity(0.6))
                        .on_press(move || remove(path.clone()))
                        .child(
                            text("移除")
                                .text_size(px(11.0))
                                .text_color(move || theme().muted_foreground),
                        ),
                )
        },
    ));

    // One centred column, kept to a readable width: macOS apps do not stretch
    // a single form across a wide window, and neither does this one.
    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(
            // A plain view: this app runs on the desktop, where the safe area
            // is zero, and `safe_area_view` is gone from NSH as of 0.4.
            view().flex_1().items_center().justify_center().p_8().child(
                // A comfortable measure that also shrinks with the window:
                // a fixed width wider than the space clips its right side.
                view()
                    .w_full()
                    .max_w(px(500.0))
                    .gap_6()
                    .child(
                        // Header: title, then one line of explanation.
                        view()
                            .gap_1()
                            .child(
                                text("轻影")
                                    .text_size(px(22.0))
                                    .text_color(move || theme().foreground),
                            )
                            .child(
                                text(
                                    "拖入视频或整个文件夹，也可以选多个。带透明通道的会保留透明。",
                                )
                                .text_size(px(12.0))
                                .text_color(move || theme().muted_foreground),
                            ),
                    )
                    // Drop zone: the whole target, with its own state text.
                    .child(
                        view()
                            .test_id("dropzone")
                            .w_full()
                            // Fill what is left between the header and
                            // the footer, but stop well before the window
                            // turns into one huge empty target.
                            .flex_1()
                            .min_h(px(200.0))
                            .max_h(px(340.0))
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .rounded(px(14.0))
                            .border(px(1.5))
                            .border_color(move || {
                                if hovering.get() {
                                    theme().primary
                                } else {
                                    theme().border
                                }
                            })
                            .bg(move || {
                                if hovering.get() {
                                    theme().muted
                                } else {
                                    theme().background
                                }
                            })
                            .on_file_drag_enter(move || hovering.set(true))
                            .on_file_drag_leave(move || hovering.set(false))
                            .on_file_drop(move |event: &FileDropEvent| {
                                hovering.set(false);
                                trace(&format!("[drop] uris = {:?}", event.uris));
                                let paths: Vec<PathBuf> = event
                                    .uris
                                    .iter()
                                    .filter_map(|uri| path_from_uri(uri))
                                    .collect();
                                if paths.is_empty() {
                                    trace("[drop] no usable path in the drop");
                                    job.set(Job::Failed {
                                        message: "读不出拖进来的内容，试试点「选择文件」".into(),
                                    });
                                } else {
                                    start(paths);
                                }
                            })
                            .child(
                                text(move || match job.get() {
                                    Job::Idle => "把视频拖到这里".to_string(),
                                    Job::Scanning => "正在找视频…".to_string(),
                                    Job::Ready { files } => {
                                        format!("{} 个文件待压缩", files.len())
                                    }
                                    Job::Running { total: _, name, .. } => name,
                                    Job::Finished { .. } => "再拖一批".to_string(),
                                    Job::Failed { .. } => "再试一次".to_string(),
                                })
                                .text_size(px(15.0))
                                .text_color(move || {
                                    match job.get() {
                                        Job::Idle | Job::Ready { .. } => theme().muted_foreground,
                                        Job::Failed { .. } => theme().destructive,
                                        _ => theme().foreground,
                                    }
                                }),
                            )
                            // While it runs: which file, how far, what codec.
                            .child(when(
                                move || matches!(job.get(), Job::Running { .. }),
                                move || {
                                    view()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                text(move || match job.get() {
                                                    Job::Running {
                                                        total,
                                                        done,
                                                        progress,
                                                        alpha,
                                                        ..
                                                    } => {
                                                        let codec = if alpha {
                                                            "HEVC + 透明"
                                                        } else {
                                                            "H.264"
                                                        };
                                                        if total > 1 {
                                                            format!(
                                                                "{} / {}  ·  {:.0}%  ·  {}",
                                                                done + 1,
                                                                total,
                                                                progress * 100.0,
                                                                codec
                                                            )
                                                        } else {
                                                            format!(
                                                                "{:.0}%  ·  {}",
                                                                progress * 100.0,
                                                                codec
                                                            )
                                                        }
                                                    }
                                                    _ => String::new(),
                                                })
                                                .text_size(px(11.0))
                                                .text_color(move || theme().muted_foreground),
                                            )
                                            .child(
                                                view()
                                                    .w(px(200.0))
                                                    .h(px(4.0))
                                                    .rounded(px(2.0))
                                                    .bg(move || theme().border)
                                                    .child(
                                                        view()
                                                            .h(px(4.0))
                                                            .rounded(px(2.0))
                                                            .w(move || {
                                                                let fraction =
                                                                    match job.get() {
                                                                        Job::Running {
                                                                            total,
                                                                            done,
                                                                            progress,
                                                                            ..
                                                                        } => {
                                                                            (done as f64
                                                                                + progress)
                                                                                / total.max(1)
                                                                                    as f64
                                                                        }
                                                                        _ => 0.0,
                                                                    };
                                                                relative(
                                                                    fraction.clamp(0.0, 1.0)
                                                                        as f32,
                                                                )
                                                            })
                                                            .bg(move || theme().primary),
                                                    ),
                                            )
                                            // Stop: the file in flight is
                                            // killed and the rest are left
                                            // as they are.
                                            .child(
                                                view()
                                                    .px_3()
                                                    .py_1()
                                                    .rounded(px(6.0))
                                                    .border(px(1.0))
                                                    .border_color(move || theme().border)
                                                    .active(|s| s.opacity(0.6))
                                                    .on_press(move || stop())
                                                    .child(
                                                        text("中断")
                                                            .text_size(px(11.0))
                                                            .text_color(move || {
                                                                theme().foreground
                                                            }),
                                                    ),
                                            )
                                },
                            )),
                    )
                    // The waiting list: what will be encoded, one row each,
                    // so a file the user no longer wants can come back out
                    // before anything is spent on it.
                    .child(when(
                        move || matches!(job.get(), Job::Ready { .. }),
                        move || {
                            view()
                                .w_full()
                                .gap_2()
                                // A long list scrolls instead of pushing the
                                // buttons out of the window.
                                .child(scroll().max_h(px(180.0)).child(list.clone()))
                                // Start it, or drop the whole list.
                                .child(
                                    view()
                                        .flex_row()
                                        .items_center()
                                        .gap_2()
                                        .child(button("开始压缩", true, move || begin()))
                                        .child(button("清空", false, move || clear())),
                                )
                        },
                    ))
                    // Result: what came out, where it went, what to do next.
                    .child(when(
                        move || matches!(job.get(), Job::Finished { .. }),
                        move || {
                            let out = move || match job.get() {
                                Job::Finished { last_out, .. } => last_out,
                                _ => None,
                            };
                            view()
                                .w_full()
                                .p_4()
                                .gap_3()
                                .rounded(px(12.0))
                                .bg(move || theme().muted)
                                // The tallies: how many, and how much smaller.
                                .child(
                                    text(move || match job.get() {
                                        Job::Finished {
                                            total,
                                            ok,
                                            saved_from,
                                            saved_to,
                                            stopped,
                                            ..
                                        } => {
                                            let saved = if saved_from > 0 {
                                                format!(
                                                    "{}  →  {}    省了 {:.0}%",
                                                    fmt_bytes(saved_from),
                                                    fmt_bytes(saved_to),
                                                    (1.0 - saved_to as f64
                                                        / saved_from.max(1) as f64)
                                                        * 100.0
                                                )
                                            } else {
                                                String::new()
                                            };
                                            // A stopped run says so, and
                                            // names what did get through
                                            // rather than pretending the
                                            // whole batch was tried.
                                            let head = if stopped {
                                                format!("已中断 · 完成 {ok} / {total}")
                                            } else if total > 1 {
                                                format!("{ok} / {total} 个文件")
                                            } else {
                                                String::new()
                                            };
                                            match (head.is_empty(), saved.is_empty()) {
                                                (true, _) => saved,
                                                (false, true) => head,
                                                (false, false) => format!("{head}   {saved}"),
                                            }
                                        }
                                        _ => String::new(),
                                    })
                                    .text_size(px(13.0))
                                    .text_color(move || theme().foreground),
                                )
                                // Failures worth naming, without hiding the rest.
                                .child(when(
                                    move || {
                                        matches!(
                                            job.get(),
                                            Job::Finished {
                                                first_error: Some(_),
                                                ..
                                            }
                                        )
                                    },
                                    move || {
                                        text(move || match job.get() {
                                            Job::Finished {
                                                total,
                                                ok,
                                                first_error: Some(error),
                                                ..
                                            } => {
                                                format!("{} 个失败：{error}", total - ok)
                                            }
                                            _ => String::new(),
                                        })
                                        .text_size(px(11.0))
                                        .text_color(move || theme().destructive)
                                    },
                                ))
                                .child(
                                    view()
                                        .flex_row()
                                        .gap_2()
                                        .child(button_with_icon(
                                            "在访达中显示",
                                            Icon::Reveal,
                                            true,
                                            move || {
                                                if let Some(p) = out() {
                                                    reveal(&p)
                                                }
                                            },
                                        ))
                                        .child(button_with_icon(
                                            "打开",
                                            Icon::Play,
                                            false,
                                            move || {
                                                if let Some(p) = out() {
                                                    open_path(&p)
                                                }
                                            },
                                        )),
                                )
                        },
                    ))
                    // The error, when there is one.
                    .child(when(
                        move || matches!(job.get(), Job::Failed { .. }),
                        move || {
                            text(move || match job.get() {
                                Job::Failed { message } => message,
                                _ => String::new(),
                            })
                            .text_size(px(12.0))
                            .text_color(move || theme().destructive)
                        },
                    ))
                    // Footer: quality on the left, the action on the right.
                    .child(
                        view()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                text("画质")
                                    .text_size(px(12.0))
                                    .text_color(move || theme().muted_foreground),
                            )
                            .child(
                                view()
                                    .flex_row()
                                    .gap_1()
                                    .p_1()
                                    .rounded(px(8.0))
                                    .bg(move || theme().muted)
                                    .children(
                                        [Quality::Small, Quality::Balanced, Quality::High].map(
                                            |q| {
                                                // Each preset says what it
                                                // costs and buys, so the
                                                // three are a choice instead
                                                // of three words.
                                                view()
                                                    .test_id(match q {
                                                        Quality::Small => "quality-small",
                                                        Quality::Balanced => "quality-balanced",
                                                        Quality::High => "quality-high",
                                                    })
                                                    .px_3()
                                                    .py_1()
                                                    .rounded(px(6.0))
                                                    .bg(move || {
                                                        if quality.get() == q {
                                                            theme().primary
                                                        } else {
                                                            theme().muted
                                                        }
                                                    })
                                                    .active(|s| s.opacity(0.7))
                                                    .tooltip(
                                                        tooltip().delay(350.0).child(
                                                            view()
                                                                .w(px(210.0))
                                                                .p_2()
                                                                .rounded(px(6.0))
                                                                .bg(move || theme().popover)
                                                                .child(
                                                                    text(q.hint())
                                                                        .text_size(px(11.0))
                                                                        .text_color(move || {
                                                                            theme()
                                                                                .popover_foreground
                                                                        }),
                                                                ),
                                                        ),
                                                    )
                                                    .on_press(move || quality.set(q))
                                                    .child(
                                                        text(q.label())
                                                            .text_size(px(12.0))
                                                            .text_color(move || {
                                                                if quality.get() == q {
                                                                    theme().primary_foreground
                                                                } else {
                                                                    theme().muted_foreground
                                                                }
                                                            }),
                                                    )
                                            },
                                        ),
                                    ),
                            )
                            .child(view().flex_1())
                            // Built here rather than with `button()`: its
                            // label follows the state (a waiting list is
                            // added to, an empty window is filled).
                            .child(
                                view()
                                    .px_4()
                                    .py_2()
                                    .rounded(px(8.0))
                                    .bg(move || theme().primary)
                                    .active(|s| s.opacity(0.7))
                                    .on_press(move || browse())
                                    .child(
                                        text(move || match job.get() {
                                            Job::Ready { .. } => "添加文件".to_string(),
                                            _ => "选择文件".to_string(),
                                        })
                                        .text_size(px(13.0))
                                        .text_color(move || theme().primary_foreground),
                                    ),
                            ),
                    )
                    // Version, and a manual check against GitHub's latest
                    // release. The result shows in this same row.
                    .child(
                        view()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                text({
                                    let version = current_version.clone();
                                    move || format!("轻影 {version}")
                                })
                                .text_size(px(11.0))
                                .text_color(move || theme().muted_foreground),
                            )
                            .child(
                                view()
                                    .rounded(px(6.0))
                                    .px_2()
                                    .py_1()
                                    .active(|s| s.opacity(0.6))
                                    .on_press(move || check_now())
                                    .child(
                                        text(move || match update.get() {
                                            UpdateCheck::Idle => "检查更新".to_string(),
                                            UpdateCheck::Checking => "检查中…".to_string(),
                                            UpdateCheck::UpToDate { .. } => "已是新版".to_string(),
                                            UpdateCheck::Available { latest, .. } => {
                                                format!("有新版本 {latest}")
                                            }
                                            // `fetch` hands back the whole
                                            // body, so there is no running
                                            // byte count to show. Before
                                            // any bytes arrive the label
                                            // says only that it is working —
                                            // a "0.0 MB" that cannot move
                                            // reads as stuck, which is what
                                            // sent us looking for this bug.
                                            UpdateCheck::Downloading { received, .. } => {
                                                if received > 0 {
                                                    let mb = received as f64 / 1048576.0;
                                                    format!("下载中 {mb:.1} MB")
                                                } else {
                                                    "正在下载…".to_string()
                                                }
                                            }
                                            UpdateCheck::Installed { .. } => "已更新".to_string(),
                                            UpdateCheck::Failed { .. } => {
                                                "检查失败，重试".to_string()
                                            }
                                        })
                                        .text_size(px(11.0))
                                        .text_color(
                                            move || match update.get() {
                                                UpdateCheck::Available { .. }
                                                | UpdateCheck::Installed { .. } => theme().primary,
                                                UpdateCheck::Failed { .. } => theme().destructive,
                                                _ => theme().muted_foreground,
                                            },
                                        ),
                                    ),
                            )
                            .child(view().flex_1())
                            // The outcome, and the way to it.
                            .child(when(
                                move || matches!(update.get(), UpdateCheck::Failed { .. }),
                                move || {
                                    text(move || match update.get() {
                                        UpdateCheck::Failed { message } => message,
                                        _ => String::new(),
                                    })
                                    .text_size(px(10.0))
                                    .text_color(move || theme().destructive)
                                },
                            ))
                            // Update: download and install in place.
                            .child(when(
                                move || matches!(update.get(), UpdateCheck::Available { .. }),
                                move || {
                                    view()
                                        .rounded(px(6.0))
                                        .px_2()
                                        .py_1()
                                        .bg(move || theme().primary)
                                        .active(|s| s.opacity(0.7))
                                        .on_press(move || install_now())
                                        .child(
                                            text("立即更新")
                                                .text_size(px(11.0))
                                                .text_color(move || theme().primary_foreground),
                                        )
                                },
                            ))
                            // Installed: the new bundle is on disk, the
                            // running process is still the old code.
                            .child(when(
                                move || matches!(update.get(), UpdateCheck::Installed { .. }),
                                move || {
                                    view()
                                        .flex_row()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            text("已装好，重启生效")
                                                .text_size(px(10.0))
                                                .text_color(move || theme().muted_foreground),
                                        )
                                        .child(
                                            view()
                                                .rounded(px(6.0))
                                                .px_2()
                                                .py_1()
                                                .bg(move || theme().primary)
                                                .active(|s| s.opacity(0.7))
                                                .on_press(move || {
                                                    let exe = std::env::current_exe().ok();
                                                    let bundle = bundle_name();
                                                    // The new copy is in
                                                    // /Applications already.
                                                    let _ = std::process::Command::new("open")
                                                        .arg(format!("/Applications/{bundle}"))
                                                        .status();
                                                    let _ = exe;
                                                    // Quit so the fresh
                                                    // bundle is what runs.
                                                    std::process::exit(0);
                                                })
                                                .child(
                                                    text("重启").text_size(px(11.0)).text_color(
                                                        move || theme().primary_foreground,
                                                    ),
                                                ),
                                        )
                                },
                            )),
                    ),
            ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The feed's newest entry is what "is there a newer release" reads.
    /// The download itself is covered by `tests/updater.rs`, which drives
    /// the real app through a stubbed network.
    #[test]
    fn a_newer_release_is_detected_from_the_feed() {
        let feed = r#"<feed><entry><title>v9.9.9</title>
            <link href="https://github.com/x/y/releases/tag/v9.9.9"/></entry></feed>"#;
        let (tag, url) = parse_latest_release(feed).expect("parse");
        assert_eq!(tag, "v9.9.9");
        assert!(url.contains("v9.9.9"));
        assert!(is_newer(&tag, "1.5.0"));
        assert!(!is_newer("1.5.0", "1.5.0"));
    }

    /// ffmpeg overwrites one live report with the next using `\r`; treating
    /// only `\n` as a separator used to leave the progress bar at 0% until
    /// the encode was over. A CRLF pair must not add an empty record either.
    #[test]
    fn carriage_return_stats_are_separate_progress_records() {
        let output = concat!(
            "ffmpeg version 8.0\n",
            "frame=   0 ... time=N/A ...\r",
            "frame=  30 ... time=00:00:01.00 ...\r",
            "frame=  60 ... time=00:00:02.00 ...\r\n",
            "[libx264 @ 0x1] done\n",
            "frame=  90 ... time=00:00:03.00 ...",
        );
        let (tx, rx) = std::sync::mpsc::channel();
        stream_ffmpeg_output(output.as_bytes(), tx);
        let records: Vec<String> = rx.into_iter().collect();

        assert_eq!(records.len(), 6);
        assert_eq!(progress_percent(&records[1], 4.0), None);
        assert_eq!(progress_percent(&records[2], 4.0), Some(0.25));
        assert_eq!(progress_percent(&records[3], 4.0), Some(0.5));
        // The final report has no newline after it; it still arrives.
        assert_eq!(progress_percent(&records[5], 4.0), Some(0.75));
    }

    #[test]
    fn progress_stays_within_the_file_and_ignores_unavailable_time() {
        assert_eq!(progress_percent("frame= 1 ... time=N/A ...", 10.0), None);
        assert_eq!(
            progress_percent("frame= 1 ... time=00:00:30.00 ...", 10.0),
            Some(1.0)
        );
    }
}
