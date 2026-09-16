//! Preview extraction.
//!
//! Everything here is bounded: at most 8 KiB is read to decide what a file is,
//! at most `max_text_bytes` is ever held in memory, images are identified from
//! their header without decoding pixels, and the poppler subprocesses are killed
//! if they overrun their timeout. Any failure degrades to [`Preview::Meta`].

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;

/// How much of a file is read to classify it.
const SNIFF_BYTES: usize = 8 * 1024;
/// Wall-clock budget for each poppler invocation.
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preview {
    /// A text file's leading bytes.
    Text {
        content: String,
        truncated: bool,
        lines: usize,
    },
    /// A decodable image, ready to be loaded from `path` by the UI.
    /// SVGs land here too; their `width`/`height` are best-effort (0 when
    /// the root tag declares none).
    Image {
        path: PathBuf,
        width: u32,
        height: u32,
    },
    /// An audio stream, ready to be played from `path` by the UI.
    Audio {
        path: PathBuf,
    },
    /// First page rendered to `page_png` (in the cache dir) plus extracted text.
    Pdf {
        page_png: PathBuf,
        text: String,
        pages: usize,
    },
    /// Fallback card, always available.
    Meta(FileMeta),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    pub owner: String,
    /// Human-readable type, e.g. `"directory"`, `"pdf"`, `"symlink"`.
    pub kind: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct PreviewLimits {
    pub max_text_bytes: usize,
    pub max_image_bytes: u64,
    pub cache_dir: PathBuf,
}

impl Default for PreviewLimits {
    fn default() -> Self {
        PreviewLimits {
            max_text_bytes: 64 * 1024,
            max_image_bytes: 64 * 1024 * 1024,
            cache_dir: option_sdk::App::SEARCH.cache_dir(),
        }
    }
}

impl PreviewLimits {
    pub fn from_config(config: &crate::Config) -> Self {
        PreviewLimits {
            cache_dir: config.cache_dir.clone(),
            ..Default::default()
        }
    }
}

/// Builds a preview for `path`. Never panics; failures become [`Preview::Error`].
pub fn preview(path: &Path, limits: &PreviewLimits) -> Preview {
    let meta = match file_meta(path) {
        Ok(m) => m,
        Err(e) => return Preview::Error(format!("{}: {e}", path.display())),
    };
    // Directories, symlinks and empty files have nothing to extract; the card is
    // more informative than an empty pane.
    if meta.is_dir || meta.kind == "symlink" || meta.size == 0 {
        return Preview::Meta(meta);
    }

    let head = match read_head(path, SNIFF_BYTES) {
        Ok(h) => h,
        Err(e) => return Preview::Error(format!("{}: {e}", path.display())),
    };

    if head.starts_with(b"%PDF-") {
        if let Some(p) = pdf_preview(path, &meta, limits) {
            return p;
        }
        return Preview::Meta(meta);
    }
    // SVG is text, so it must be claimed before the raster and text checks.
    if is_svg(path, &head) {
        if let Some(p) = svg_preview(path, &meta, &head, limits) {
            return p;
        }
        return Preview::Meta(meta);
    }
    if image::guess_format(&head).is_ok() {
        if let Some(p) = image_preview(path, &meta, limits) {
            return p;
        }
        return Preview::Meta(meta);
    }
    if is_audio(path, &head) {
        return Preview::Audio {
            path: path.to_path_buf(),
        };
    }
    if looks_like_text(&head) {
        if let Some(p) = text_preview(path, limits) {
            return p;
        }
    }
    Preview::Meta(meta)
}

fn read_head(path: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(max.min(SNIFF_BYTES));
    f.take(max as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

// ---- text ----------------------------------------------------------------

/// A sniff window is text when it has no NUL and decodes as UTF-8. A multi-byte
/// character cut by the window boundary is not a decode failure.
fn looks_like_text(head: &[u8]) -> bool {
    if head.is_empty() || memchr::memchr(0, head).is_some() {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

fn text_preview(path: &Path, limits: &PreviewLimits) -> Option<Preview> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(limits.max_text_bytes as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;

    let mut truncated = buf.len() > limits.max_text_bytes;
    buf.truncate(limits.max_text_bytes);
    let valid = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        Err(e) => {
            truncated = true;
            e.valid_up_to()
        }
    };
    buf.truncate(valid);
    let content = String::from_utf8(buf).ok()?;
    let lines = content.lines().count();
    Some(Preview::Text {
        content,
        truncated,
        lines,
    })
}

// ---- images --------------------------------------------------------------

fn image_preview(path: &Path, meta: &FileMeta, limits: &PreviewLimits) -> Option<Preview> {
    if meta.size > limits.max_image_bytes {
        return None;
    }
    // Reads the header only: a 100 MP photo costs the same as a thumbnail.
    let (width, height) = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some(Preview::Image {
        path: path.to_path_buf(),
        width,
        height,
    })
}

// ---- svg -----------------------------------------------------------------

/// SVG needs both sides to agree: a name that says "svg" and an `<svg` tag in
/// the sniff window. That keeps an `.xml`/`.html` file with an inline SVG on
/// the text path, and an SVG-looking blob with a random name off it.
fn is_svg(path: &Path, head: &[u8]) -> bool {
    if !head.windows(4).any(|w| w == b"<svg") {
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
        || mime_guess::from_path(path)
            .first()
            .is_some_and(|m| m.essence_str() == "image/svg+xml")
}

/// SVGs are decoded by the UI's image loader, so the preview only needs the
/// path plus whatever dimensions the root tag declares (`width`/`height`,
/// else `viewBox`). Missing or unparseable sizes degrade to `0`.
fn svg_preview(
    path: &Path,
    meta: &FileMeta,
    head: &[u8],
    limits: &PreviewLimits,
) -> Option<Preview> {
    if meta.size > limits.max_image_bytes {
        return None;
    }
    let (width, height) = svg_dimensions(head);
    Some(Preview::Image {
        path: path.to_path_buf(),
        width,
        height,
    })
}

fn svg_dimensions(head: &[u8]) -> (u32, u32) {
    let Ok(text) = std::str::from_utf8(head) else {
        return (0, 0);
    };
    let Some(start) = text.find("<svg") else {
        return (0, 0);
    };
    let rest = &text[start..];
    let tag = &rest[..rest.find('>').unwrap_or(rest.len()).min(2048)];

    // `width`/`height` win; `viewBox="min-x min-y w h"` is the fallback.
    if let (Some(w), Some(h)) = (svg_attr(tag, "width"), svg_attr(tag, "height")) {
        return (w as u32, h as u32);
    }
    if let Some(vb) = svg_attr_str(tag, "viewBox") {
        let mut nums = vb
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<f64>().ok());
        if let (Some(w), Some(h)) = (nums.nth(2), nums.next()) {
            return (w as u32, h as u32);
        }
    }
    (0, 0)
}

/// Reads `name="value"` / `name='value'` out of an open tag, requiring a
/// whitespace boundary so `stroke-width` never answers for `width`.
fn svg_attr_str<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    for (i, _) in tag.match_indices(name) {
        if i > 0 && !tag.as_bytes()[i - 1].is_ascii_whitespace() {
            continue;
        }
        let v = tag[i + name.len()..].trim_start();
        let Some(v) = v.strip_prefix('=') else {
            continue;
        };
        let v = v.trim_start();
        let Some(q) = v.chars().next() else { continue };
        if q != '"' && q != '\'' {
            continue;
        }
        let inner = &v[q.len_utf8()..];
        let Some(end) = inner.find(q) else { continue };
        return Some(&inner[..end]);
    }
    None
}

fn svg_attr(tag: &str, name: &str) -> Option<f64> {
    let raw = svg_attr_str(tag, name)?.trim();
    // Sizes may carry units (`px`, `pt`, `%`); only unitless/px-ish numbers
    // map to pixels, the rest parse after trimming the suffix anyway and are
    // close enough for a metadata line.
    let raw = raw.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%');
    raw.trim().parse().ok()
}

// ---- audio ---------------------------------------------------------------

/// Magic bytes cover extensionless files; `mime_guess` covers the long tail
/// of extensions. OggS is left to the mime check so `.ogv` stays video.
fn is_audio(path: &Path, head: &[u8]) -> bool {
    if head.starts_with(b"fLaC")
        || head.starts_with(b"ID3")
        || head.starts_with(b"\xff\xfb")
        || head.starts_with(b"\xff\xf3")
        || head.starts_with(b"\xff\xf2")
        || (head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE")
        || (head.len() >= 12
            && &head[..4] == b"FORM"
            && (&head[8..12] == b"AIFF" || &head[8..12] == b"AIFC"))
    {
        return true;
    }
    mime_guess::from_path(path)
        .first()
        .is_some_and(|m| m.type_() == "audio")
}

// ---- pdf -----------------------------------------------------------------

/// Renders page 1 through poppler into the cache directory.
///
/// The cache is keyed by path, mtime and size, so re-selecting a PDF skips both
/// subprocesses entirely. Returns `None` when poppler is missing or fails, which
/// the caller turns into a metadata card.
fn pdf_preview(path: &Path, meta: &FileMeta, limits: &PreviewLimits) -> Option<Preview> {
    let dir = limits.cache_dir.join("pdf");
    std::fs::create_dir_all(&dir).ok()?;
    let key = cache_key(path, meta.mtime, meta.size);
    let png = dir.join(format!("{key}.png"));
    let txt = dir.join(format!("{key}.txt"));
    let pages_file = dir.join(format!("{key}.pages"));

    if !png.exists() {
        // pdftoppm appends `.png` to the prefix; render to a private name so two
        // threads previewing the same PDF cannot tear each other's output.
        let stem = dir.join(format!("{key}.{}.part", std::process::id()));
        let rendered = dir.join(format!("{key}.{}.part.png", std::process::id()));
        let ok = run(Command::new("pdftoppm")
            .args(["-png", "-f", "1", "-l", "1", "-r", "96", "-singlefile"])
            .arg(path)
            .arg(&stem));
        if !ok || std::fs::rename(&rendered, &png).is_err() {
            let _ = std::fs::remove_file(&rendered);
            return None;
        }
    }

    let text = match std::fs::read_to_string(&txt) {
        Ok(t) => t,
        Err(_) => {
            run(Command::new("pdftotext")
                .args(["-l", "1"])
                .arg(path)
                .arg(&txt));
            std::fs::read_to_string(&txt).unwrap_or_default()
        }
    };

    let pages = match std::fs::read_to_string(&pages_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
    {
        Some(n) => n,
        None => {
            let n = pdf_page_count(path).unwrap_or(1);
            let _ = std::fs::write(&pages_file, n.to_string());
            n
        }
    };

    Some(Preview::Pdf {
        page_png: png,
        text: truncate_utf8(text, limits.max_text_bytes),
        pages,
    })
}

fn pdf_page_count(path: &Path) -> Option<usize> {
    let mut child = Command::new("pdfinfo")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let status = wait_with_timeout(&mut child, SUBPROCESS_TIMEOUT)?;
    if !status.success() {
        return None;
    }
    // pdfinfo prints a handful of short lines, far below the pipe buffer, so
    // reading after the exit cannot deadlock.
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    out.lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|v| v.trim().parse().ok())
}

/// Runs a command with no stdio, killing it if it overruns the timeout.
fn run(cmd: &mut Command) -> bool {
    let Ok(mut child) = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
        return false;
    };
    wait_with_timeout(&mut child, SUBPROCESS_TIMEOUT).is_some_and(|s| s.success())
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// FNV-1a over path + mtime + size. Hand-rolled so cached renders survive a
/// toolchain upgrade, unlike `DefaultHasher`.
fn cache_key(path: &Path, mtime: i64, size: u64) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    };
    eat(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::as_bytes(
        path.as_os_str(),
    ));
    eat(&mtime.to_le_bytes());
    eat(&size.to_le_bytes());
    format!("{h:016x}")
}

fn truncate_utf8(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Collects the metadata card shown for any file.
pub fn file_meta(path: &Path) -> std::io::Result<FileMeta> {
    let md = std::fs::symlink_metadata(path)?;
    let is_dir = md.is_dir();
    let kind = if md.file_type().is_symlink() {
        "symlink".to_string()
    } else if is_dir {
        "directory".to_string()
    } else {
        kind_from_extension(path)
    };
    Ok(FileMeta {
        path: path.to_path_buf(),
        size: md.len(),
        mtime: md.mtime(),
        mode: md.permissions().mode(),
        owner: owner_name(md.uid()),
        kind,
        is_dir,
    })
}

fn kind_from_extension(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) if !e.is_empty() => e.to_ascii_lowercase(),
        _ => read_head(path, 16).map_or_else(|_| "file".to_string(), |h| kind_from_magic(&h)),
    }
}

fn kind_from_magic(head: &[u8]) -> String {
    let magic: &[(&[u8], &str)] = &[
        (b"%PDF-", "pdf"),
        (b"\x7fELF", "elf"),
        (b"\x89PNG", "png"),
        (b"\xff\xd8\xff", "jpeg"),
        (b"GIF8", "gif"),
        (b"PK\x03\x04", "zip"),
        (b"\x1f\x8b", "gzip"),
        (b"#!", "script"),
        (b"OggS", "ogg"),
        (b"\x00asm", "wasm"),
    ];
    for (sig, kind) in magic {
        if head.starts_with(sig) {
            return kind.to_string();
        }
    }
    if head.is_empty() {
        "file".to_string()
    } else if looks_like_text(head) {
        "text".to_string()
    } else {
        "binary".to_string()
    }
}

/// Resolves a uid to a user name via `/etc/passwd`, falling back to the number.
pub fn owner_name(uid: u32) -> String {
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let mut f = line.split(':');
            let (Some(name), Some(_), Some(id)) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            if id.parse::<u32>() == Ok(uid) {
                return name.to_string();
            }
        }
    }
    uid.to_string()
}

/// Renders a mode as `rwxr-xr-x`.
pub fn mode_string(mode: u32) -> String {
    let bits = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    bits.iter()
        .map(|&(m, c)| if mode & m != 0 { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(tmp: &Path) -> PreviewLimits {
        PreviewLimits {
            cache_dir: tmp.join("cache"),
            ..Default::default()
        }
    }

    #[test]
    fn meta_preview_for_a_binary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("notes.TXT");
        std::fs::write(&f, b"he\0llo").unwrap();
        match preview(&f, &PreviewLimits::default()) {
            Preview::Meta(m) => {
                assert_eq!(m.size, 6);
                assert_eq!(m.kind, "txt");
                assert!(!m.is_dir);
                assert!(!m.owner.is_empty());
            }
            other => panic!("expected Meta, got {other:?}"),
        }
    }

    #[test]
    fn text_preview_counts_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "fn main() {\n    println!(\"oi\");\n}\n").unwrap();
        match preview(&f, &limits(tmp.path())) {
            Preview::Text {
                content,
                truncated,
                lines,
            } => {
                assert!(content.starts_with("fn main"));
                assert!(!truncated);
                assert_eq!(lines, 3);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn text_preview_handles_utf8_and_stops_at_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("acentos.txt");
        // 4 bytes per repetition, so the cut lands mid-character for odd limits.
        std::fs::write(&f, "áé".repeat(4096)).unwrap();
        let lim = PreviewLimits {
            max_text_bytes: 1001,
            ..limits(tmp.path())
        };
        match preview(&f, &lim) {
            Preview::Text {
                content, truncated, ..
            } => {
                assert!(truncated);
                assert!(content.len() <= 1001);
                assert!(content.chars().all(|c| c == 'á' || c == 'é'));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn huge_text_file_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("big.log");
        let line = "the quick brown fox jumps over the lazy dog\n";
        std::fs::write(&f, line.repeat(200_000)).unwrap();
        let started = std::time::Instant::now();
        match preview(&f, &limits(tmp.path())) {
            Preview::Text {
                content, truncated, ..
            } => {
                assert!(truncated);
                assert_eq!(content.len(), 64 * 1024);
            }
            other => panic!("expected Text, got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn empty_file_falls_back_to_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("empty.txt");
        std::fs::write(&f, b"").unwrap();
        assert!(matches!(preview(&f, &limits(tmp.path())), Preview::Meta(_)));
    }

    #[test]
    fn binary_file_is_not_text() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("blob");
        let mut bytes = vec![b'a'; 4096];
        bytes[3000] = 0;
        std::fs::write(&f, &bytes).unwrap();
        assert!(matches!(preview(&f, &limits(tmp.path())), Preview::Meta(_)));
        assert_eq!(kind_from_magic(&[0xde, 0xad, 0, 0xbe]), "binary");
        assert_eq!(kind_from_magic(b"#!/bin/sh\n"), "script");
    }

    #[test]
    fn sniffing_accepts_a_multibyte_char_cut_by_the_window() {
        let mut head = "ação ".repeat(1000).into_bytes();
        head.truncate(SNIFF_BYTES);
        assert!(looks_like_text(&head));
        assert!(!looks_like_text(b"\x89PNG\r\n\x1a\n"));
        assert!(!looks_like_text(b""));
    }

    #[test]
    fn image_preview_reports_dimensions() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("pic.png");
        let img = image::RgbImage::new(640, 360);
        img.save(&f).unwrap();
        match preview(&f, &limits(tmp.path())) {
            Preview::Image {
                width,
                height,
                path,
            } => {
                assert_eq!((width, height), (640, 360));
                assert_eq!(path, f);
            }
            other => panic!("expected Image, got {other:?}"),
        }

        let lim = PreviewLimits {
            max_image_bytes: 4,
            ..limits(tmp.path())
        };
        assert!(matches!(preview(&f, &lim), Preview::Meta(_)));
    }

    #[test]
    fn cache_key_tracks_mtime_and_size() {
        let p = Path::new("/tmp/a.pdf");
        assert_eq!(cache_key(p, 1, 2), cache_key(p, 1, 2));
        assert_ne!(cache_key(p, 1, 2), cache_key(p, 2, 2));
        assert_ne!(cache_key(p, 1, 2), cache_key(p, 1, 3));
        assert_ne!(cache_key(p, 1, 2), cache_key(Path::new("/tmp/b.pdf"), 1, 2));
    }

    #[test]
    fn malformed_pdf_degrades_to_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("broken.pdf");
        std::fs::write(&f, b"%PDF-1.7\nnot really a pdf").unwrap();
        assert!(matches!(preview(&f, &limits(tmp.path())), Preview::Meta(_)));
    }

    #[test]
    fn pdf_preview_renders_and_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let lim = limits(tmp.path());
        let f = tmp.path().join("doc.pdf");
        std::fs::write(&f, sample_pdf(3)).unwrap();
        if Command::new("pdftoppm").arg("-v").output().is_err() {
            return;
        }

        let cold = Instant::now();
        let first = preview(&f, &lim);
        let cold = cold.elapsed();
        let Preview::Pdf {
            page_png,
            text,
            pages,
        } = &first
        else {
            panic!("expected Pdf, got {first:?}");
        };
        assert!(page_png.exists());
        assert_eq!(*pages, 3);
        assert!(text.contains("Needle"), "text was {text:?}");

        let warm = Instant::now();
        let second = preview(&f, &lim);
        let warm = warm.elapsed();
        assert_eq!(first, second);
        assert!(
            warm < cold.max(Duration::from_millis(4)),
            "cached {warm:?} should beat cold {cold:?}"
        );
    }

    /// Minimal multi-page PDF with one word of text, built by hand so the test
    /// needs no fixture file.
    fn sample_pdf(pages: usize) -> Vec<u8> {
        let mut objects: Vec<String> = Vec::new();
        let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", 3 + i * 2)).collect();
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
        objects.push(format!(
            "<< /Type /Pages /Count {pages} /Kids [{}] >>",
            kids.join(" ")
        ));
        for i in 0..pages {
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> \
                 /Contents {} 0 R >>",
                4 + i * 2
            ));
            let stream = format!("BT /F1 24 Tf 20 100 Td (Needle {i}) Tj ET");
            objects.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
        }

        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn directories_report_kind() {
        let tmp = tempfile::tempdir().unwrap();
        match preview(tmp.path(), &PreviewLimits::default()) {
            Preview::Meta(m) => {
                assert!(m.is_dir);
                assert_eq!(m.kind, "directory");
            }
            other => panic!("expected Meta, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_yields_error() {
        let p = Path::new("/nonexistent/needle/xyz");
        assert!(matches!(
            preview(p, &PreviewLimits::default()),
            Preview::Error(_)
        ));
    }

    #[test]
    fn mode_rendering() {
        assert_eq!(mode_string(0o755), "rwxr-xr-x");
        assert_eq!(mode_string(0o644), "rw-r--r--");
    }

    #[test]
    fn svg_previews_as_image_with_dimensions() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("icon.svg");
        std::fs::write(
            &f,
            r#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="24" height="48"><rect/></svg>"#,
        )
        .unwrap();
        match preview(&f, &limits(tmp.path())) {
            Preview::Image { width, height, .. } => assert_eq!((width, height), (24, 48)),
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn svg_viewbox_supplies_dimensions() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("shape.SVG");
        std::fs::write(
            &f,
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 96 48"><path d="M0 0"/></svg>"#,
        )
        .unwrap();
        match preview(&f, &limits(tmp.path())) {
            Preview::Image { width, height, .. } => assert_eq!((width, height), (96, 48)),
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn svg_without_declared_size_gets_zero_dimensions() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("bare.svg");
        std::fs::write(&f, "<svg><circle r=\"4\"/></svg>").unwrap();
        match preview(&f, &limits(tmp.path())) {
            Preview::Image { width, height, .. } => assert_eq!((width, height), (0, 0)),
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn non_svg_name_with_inline_svg_stays_text() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("doc.xml");
        std::fs::write(&f, "<root><svg width=\"9\" height=\"9\"/></root>").unwrap();
        assert!(matches!(
            preview(&f, &limits(tmp.path())),
            Preview::Text { .. }
        ));
    }

    #[test]
    fn audio_extension_previews_as_audio() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("song.mp3");
        std::fs::write(&f, b"ID3\x04\x00\x00\x00\x00\x00\x00pad").unwrap();
        assert!(matches!(
            preview(&f, &limits(tmp.path())),
            Preview::Audio { .. }
        ));
    }

    #[test]
    fn extensionless_audio_is_sniffed() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("track");
        std::fs::write(&f, b"fLaC\x00\x00\x00\x22rest").unwrap();
        assert!(matches!(
            preview(&f, &limits(tmp.path())),
            Preview::Audio { .. }
        ));
    }
}
