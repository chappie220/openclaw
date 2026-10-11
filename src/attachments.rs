//! Files that come with a message (QQ images, email attachments, Web UI
//! uploads, `ask --attach`): saved in the workspace under `inbox/`, recorded
//! with the message, and shown to the model during that message's turn.

use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Largest file kept; larger ones are refused with a note to the model.
pub const MAX_FILE_BYTES: usize = 20 * 1024 * 1024;
/// Most files taken from one message.
pub const MAX_FILES: usize = 10;
/// Largest image sent to the model (provider limits are around 5 MB).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Most images sent with one message.
pub const MAX_IMAGES: usize = 4;
/// Text files up to this size are put into the message itself.
pub const MAX_INLINE_TEXT: usize = 16 * 1024;
/// Rough token cost of one image, for the context budget.
pub const IMAGE_TOKENS: usize = 1500;

/// A file as it arrived, before it is saved.
#[derive(Debug, Clone, PartialEq)]
pub struct Upload {
    pub name: String,
    /// As the sender declared it; guessed from the name when missing.
    pub mime: Option<String>,
    pub data: Vec<u8>,
}

/// A saved file, as recorded with its message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    /// Relative to the workspace.
    pub path: String,
    pub bytes: u64,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        matches!(
            self.mime.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        )
    }

    pub fn is_text(&self) -> bool {
        self.mime.starts_with("text/")
            || matches!(
                self.mime.as_str(),
                "application/json"
                    | "application/xml"
                    | "application/x-yaml"
                    | "application/toml"
                    | "application/javascript"
                    | "application/x-sh"
            )
    }

    /// One line describing the file for the model.
    pub fn describe(&self) -> String {
        format!("{} ({}, {})", self.path, self.mime, size(self.bytes))
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// The MIME type for a file name's extension; `application/octet-stream`
/// when unknown.
pub fn guess_mime(name: &str) -> String {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/x-yaml",
        "toml" => "application/toml",
        "js" => "application/javascript",
        "sh" => "application/x-sh",
        "rs" | "py" | "c" | "h" | "cpp" | "go" | "java" | "ts" | "rb" | "lua" | "ini" | "conf"
        | "cfg" => "text/plain",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "aif" | "aiff" => "audio/aiff",
        "amr" => "audio/amr",
        "silk" | "slk" => "audio/silk",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
    .into()
}

/// A file extension for a MIME type, for files saved without a name.
pub fn extension_for(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "application/pdf" => "pdf",
        "application/json" => "json",
        m if m.starts_with("text/") => "txt",
        _ => "bin",
    }
}

/// A safe file name: the last path component, without control characters,
/// at most 80 characters, never empty or a dot name.
fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = last
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim().trim_start_matches('.');
    let mut out: String = clean
        .chars()
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if out.is_empty() {
        out = "file".into();
    }
    out
}

/// FNV-1a, so the same content saves to the same name.
fn content_hash(data: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Saves `upload` as `inbox/<hash>-<name>` in `workspace`. The same file
/// sent again (an email retried, say) lands on the same path.
pub fn save(workspace: &Path, upload: &Upload) -> Result<Attachment> {
    if upload.data.len() > MAX_FILE_BYTES {
        bail!(
            "{} is {}; files over {} are not kept",
            upload.name,
            size(upload.data.len() as u64),
            size(MAX_FILE_BYTES as u64)
        );
    }
    let name = safe_name(&upload.name);
    let mime = upload
        .mime
        .as_deref()
        .map(|m| {
            m.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|m| m.contains('/') && m != "application/octet-stream")
        .unwrap_or_else(|| guess_mime(&name));
    let path = format!("inbox/{}-{name}", &content_hash(&upload.data)[..12]);
    let full = workspace.join(&path);
    if let Some(dir) = full.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(&full, &upload.data)
        .with_context(|| format!("cannot write {}", full.display()))?;
    Ok(Attachment {
        name,
        mime,
        path,
        bytes: upload.data.len() as u64,
    })
}

/// Saves what fits and returns notes for the model about what did not.
pub fn save_all(workspace: &Path, uploads: &[Upload]) -> (Vec<Attachment>, Vec<String>) {
    let mut saved = Vec::new();
    let mut problems = Vec::new();
    for (index, upload) in uploads.iter().enumerate() {
        if index >= MAX_FILES {
            problems.push(format!(
                "{} more files were not kept (at most {MAX_FILES} per message)",
                uploads.len() - MAX_FILES
            ));
            break;
        }
        match save(workspace, upload) {
            Ok(attachment) => saved.push(attachment),
            Err(err) => problems.push(format!("{err:#}")),
        }
    }
    (saved, problems)
}

/// `text` with a note about files that could not be kept.
pub fn with_problems(text: &str, problems: &[String]) -> String {
    if problems.is_empty() {
        return text.to_owned();
    }
    format!(
        "{text}\n\n[Some files sent with this message could not be kept: {}]",
        problems.join("; ")
    )
}

/// What the model is shown for a message's files: a list, and during the
/// message's own turn also small text files inline and images to look at.
pub struct Shown {
    pub note: String,
    /// `data:` URLs of the images to send.
    pub images: Vec<String>,
}

/// Renders `files` for the model; `current` adds inline text and images.
pub fn show(workspace: &Path, files: &[Attachment], current: bool) -> Shown {
    let mut note = String::from(
        "[Files sent with this message, saved in the workspace; read them with tools if needed:]",
    );
    let mut images = Vec::new();
    for file in files {
        note.push_str(&format!("\n- {}", file.describe()));
        if !current {
            continue;
        }
        let full = workspace.join(&file.path);
        if file.is_image() {
            if images.len() >= MAX_IMAGES {
                note.push_str(" (not shown: too many images in one message)");
            } else if file.bytes as usize > MAX_IMAGE_BYTES {
                note.push_str(" (not shown: image too large)");
            } else if let Ok(data) = std::fs::read(&full) {
                let encoded = base64::engine::general_purpose::STANDARD.encode(data);
                images.push(format!("data:{};base64,{encoded}", file.mime));
                note.push_str(&format!(" (shown as image {})", images.len()));
            }
        } else if file.is_text()
            && file.bytes as usize <= MAX_INLINE_TEXT
            && let Ok(text) = std::fs::read_to_string(&full)
        {
            note.push_str(&format!(":\n```\n{}\n```", text.trim_end()));
        }
    }
    Shown { note, images }
}

/// Starts a tool result that carries an image for the model to look at.
const TOOL_IMAGE: &str = "[image: ";

/// The first line of a tool result that shows the model the image at
/// `path` (relative to the workspace) during the turn that made it.
pub fn tool_image_line(path: &str) -> String {
    format!("{TOOL_IMAGE}{path}]")
}

/// The image a tool result carries. Only its first line counts, and only a
/// plain path inside the workspace, so page text a tool passes on cannot
/// point the model at other files.
pub fn tool_image(output: &str) -> Option<&str> {
    let path = output
        .lines()
        .next()?
        .strip_prefix(TOOL_IMAGE)?
        .strip_suffix(']')?;
    let inside = !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    inside.then_some(path)
}

/// The `data:` URL of an image file in the workspace, or why it is not shown.
pub fn image_data_url(workspace: &Path, path: &str) -> Result<String, &'static str> {
    let mime = guess_mime(path);
    if !matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) {
        return Err("not an image");
    }
    let data = std::fs::read(workspace.join(path)).map_err(|_| "file is gone")?;
    if data.len() > MAX_IMAGE_BYTES {
        return Err("image too large");
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    Ok(format!("data:{mime};base64,{encoded}"))
}

/// Estimated tokens `show` adds for `files` in their own turn.
pub fn show_tokens(files: &[Attachment]) -> usize {
    files
        .iter()
        .map(|f| {
            30 + if f.is_image() {
                IMAGE_TOKENS
            } else if f.is_text() && f.bytes as usize <= MAX_INLINE_TEXT {
                f.bytes as usize / 3
            } else {
                0
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_images_stay_inside_the_workspace() {
        let line = tool_image_line("screenshots/a.png");
        assert_eq!(
            tool_image(&format!("{line}\nsaved")),
            Some("screenshots/a.png")
        );
        for bad in [
            "[image: ../secret.png]",
            "[image: /etc/x.png]",
            "[image: ]",
            "Title: x\n[image: a.png]",
        ] {
            assert_eq!(tool_image(bad), None, "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), b"PNG").unwrap();
        assert_eq!(
            image_data_url(dir.path(), "a.png").unwrap(),
            "data:image/png;base64,UE5H"
        );
        assert!(image_data_url(dir.path(), "missing.png").is_err());
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        assert!(image_data_url(dir.path(), "a.txt").is_err());
    }

    fn upload(name: &str, mime: Option<&str>, data: &[u8]) -> Upload {
        Upload {
            name: name.into(),
            mime: mime.map(str::to_owned),
            data: data.to_vec(),
        }
    }

    #[test]
    fn names_are_made_safe_and_unique_by_content() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\Users\\me\\报告.pdf"), "报告.pdf");
        assert_eq!(safe_name(".bashrc"), "bashrc");
        assert_eq!(safe_name("a\u{0}b?.txt"), "ab_.txt");
        assert_eq!(safe_name(""), "file");
        assert_eq!(
            safe_name(&format!("{}.txt", "x".repeat(200)))
                .chars()
                .count(),
            80
        );

        let dir = tempfile::tempdir().unwrap();
        let a = save(dir.path(), &upload("../x.txt", None, b"one")).unwrap();
        let again = save(dir.path(), &upload("x.txt", None, b"one")).unwrap();
        let other = save(dir.path(), &upload("x.txt", None, b"two")).unwrap();
        assert_eq!(a, again);
        assert_ne!(a.path, other.path);
        assert!(a.path.starts_with("inbox/") && a.path.ends_with("-x.txt"));
        assert_eq!(a.mime, "text/plain");
        assert_eq!(std::fs::read(dir.path().join(&a.path)).unwrap(), b"one");
    }

    #[test]
    fn mime_comes_from_the_sender_or_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let declared = save(dir.path(), &upload("p", Some("IMAGE/JPEG; q=1"), b"j")).unwrap();
        assert_eq!(declared.mime, "image/jpeg");
        let guessed = save(
            dir.path(),
            &upload("p.png", Some("application/octet-stream"), b"p"),
        )
        .unwrap();
        assert_eq!(guessed.mime, "image/png");
        assert!(guessed.is_image());
    }

    #[test]
    fn oversized_and_extra_files_become_notes() {
        let dir = tempfile::tempdir().unwrap();
        let mut uploads = vec![Upload {
            name: "big.bin".into(),
            mime: None,
            data: vec![0; MAX_FILE_BYTES + 1],
        }];
        for i in 0..MAX_FILES + 2 {
            uploads.push(upload(&format!("{i}.txt"), None, i.to_string().as_bytes()));
        }
        let (saved, problems) = save_all(dir.path(), &uploads);
        assert_eq!(saved.len(), MAX_FILES - 1);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("big.bin"));
    }

    #[test]
    fn shows_images_and_text_only_in_their_turn() {
        let dir = tempfile::tempdir().unwrap();
        let image = save(dir.path(), &upload("cat.png", None, b"\x89PNG")).unwrap();
        let text = save(dir.path(), &upload("notes.md", None, b"# hello")).unwrap();
        let pdf = save(dir.path(), &upload("a.pdf", None, b"%PDF")).unwrap();
        let files = [image, text, pdf];
        let now = show(dir.path(), &files, true);
        assert_eq!(now.images, ["data:image/png;base64,iVBORw=="]);
        assert!(
            now.note
                .contains("cat.png (image/png, 4 B) (shown as image 1)"),
            "{}",
            now.note
        );
        assert!(
            now.note
                .contains("notes.md (text/markdown, 7 B):\n```\n# hello\n```"),
            "{}",
            now.note
        );
        assert!(
            now.note.contains("-a.pdf (application/pdf, 4 B)"),
            "{}",
            now.note
        );
        let later = show(dir.path(), &files, false);
        assert!(later.images.is_empty());
        assert!(!later.note.contains("# hello"));
        assert!(show_tokens(&files) >= IMAGE_TOKENS);
    }
}
