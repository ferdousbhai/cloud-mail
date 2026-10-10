//! Files sent with a message: reading them, guessing their type, and how much each way of sending
//! takes.

use std::path::Path;

use crate::error::{Error, ErrorKind, Result};
use crate::text::human_size;
use crate::types::OutgoingAttachment;

/// Your Cloudmail worker: Cloudflare Email Service sends at most 5 MiB per message once encoded,
/// and base64 makes files a third bigger, so about 3.6 MiB of files fit; this leaves the body room.
pub const WORKER_LIMIT: u64 = 3584 * 1024;
/// Gmail takes 25 MB of attachments per message (the limit gws's own `+send --attach` uses).
pub const GMAIL_LIMIT: u64 = 25 * 1024 * 1024;
/// HEY, like most mail services, takes 25 MB per message.
pub const HEY_LIMIT: u64 = 25 * 1024 * 1024;
/// iCloud Mail takes messages of up to 20 MB once encoded, and base64 makes files a third bigger,
/// so about 14 MiB of files fit; this leaves the body room.
pub const ICLOUD_LIMIT: u64 = 14 * 1024 * 1024;

const TYPES: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("txt", "text/plain"),
    ("text", "text/plain"),
    ("log", "text/plain"),
    ("md", "text/markdown"),
    ("csv", "text/csv"),
    ("tsv", "text/tab-separated-values"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("css", "text/css"),
    ("ics", "text/calendar"),
    ("vcf", "text/vcard"),
    ("xml", "application/xml"),
    ("json", "application/json"),
    ("yaml", "application/yaml"),
    ("yml", "application/yaml"),
    ("toml", "application/toml"),
    ("js", "text/javascript"),
    ("rtf", "application/rtf"),
    ("eml", "message/rfc822"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("tgz", "application/gzip"),
    ("tar", "application/x-tar"),
    ("bz2", "application/x-bzip2"),
    ("xz", "application/x-xz"),
    ("zst", "application/zstd"),
    ("7z", "application/x-7z-compressed"),
    ("rar", "application/vnd.rar"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("heic", "image/heic"),
    ("svg", "image/svg+xml"),
    ("bmp", "image/bmp"),
    ("ico", "image/vnd.microsoft.icon"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("mp3", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("opus", "audio/ogg"),
    ("flac", "audio/flac"),
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("mov", "video/quicktime"),
    ("webm", "video/webm"),
    ("mkv", "video/x-matroska"),
    ("avi", "video/x-msvideo"),
    ("doc", "application/msword"),
    ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    ("xls", "application/vnd.ms-excel"),
    ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ("ppt", "application/vnd.ms-powerpoint"),
    ("pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("odp", "application/vnd.oasis.opendocument.presentation"),
    ("epub", "application/epub+zip"),
    ("key", "application/vnd.apple.keynote"),
    ("pages", "application/vnd.apple.pages"),
    ("numbers", "application/vnd.apple.numbers"),
];

/// The MIME type for a file name, by its extension; `application/octet-stream` when unknown.
pub fn mime_type(filename: &str) -> &'static str {
    let ext = Path::new(filename).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    TYPES.iter().find(|(e, _)| *e == ext).map_or("application/octet-stream", |(_, t)| t)
}

/// A file name fit for a header or a file on disk: one line, no path, not empty.
pub fn safe_filename(name: &str) -> String {
    let clean: String =
        name.chars().filter(|c| !c.is_control()).map(|c| if matches!(c, '/' | '\\') { '_' } else { c }).collect();
    let mut clean = clean.trim().to_string();
    while clean.len() > 200 {
        clean.pop();
    }
    if clean.is_empty() || clean == "." || clean == ".." { "attachment".into() } else { clean }
}

impl OutgoingAttachment {
    /// Reads a file to send, named after it and typed by its extension.
    pub fn from_path(path: &Path) -> Result<Self> {
        let fail = |why: String| Error::new(ErrorKind::BadRequest, format!("can't attach {}: {why}", path.display()));
        let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => fail("no such file".into()),
            _ => fail(e.to_string()),
        })?;
        if meta.is_dir() {
            return Err(fail("it's a directory".into()));
        }
        let content = std::fs::read(path).map_err(|e| fail(e.to_string()))?;
        let filename = safe_filename(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        Ok(Self { mime_type: mime_type(&filename).into(), filename, content })
    }

    pub fn size(&self) -> u64 {
        self.content.len() as u64
    }
}

pub fn total_size(list: &[OutgoingAttachment]) -> u64 {
    list.iter().map(OutgoingAttachment::size).sum()
}

/// An error when the files are more than `limit` bytes together.
pub fn check_limit(list: &[OutgoingAttachment], limit: u64, via: &str) -> Result<()> {
    let total = total_size(list);
    if total <= limit {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::BadRequest,
        format!(
            "the attachments are too large to send through {via}: {} in all, and it takes at most {}",
            human_size(total as i64),
            human_size(limit as i64)
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_come_from_the_extension() {
        assert_eq!(mime_type("Report.PDF"), "application/pdf");
        assert_eq!(mime_type("photo.jpeg"), "image/jpeg");
        assert_eq!(mime_type("archive.tar.gz"), "application/gzip");
        assert_eq!(mime_type("Makefile"), "application/octet-stream");
        assert_eq!(mime_type(".bashrc"), "application/octet-stream");
    }

    #[test]
    fn names_are_one_plain_line() {
        assert_eq!(safe_filename("../a/b\\c.txt"), ".._a_b_c.txt");
        assert_eq!(safe_filename("x\r\nBcc: y.txt"), "xBcc: y.txt");
        assert_eq!(safe_filename(".."), "attachment");
        assert_eq!(safe_filename("  "), "attachment");
        assert!(safe_filename(&"é".repeat(300)).len() <= 200);
    }

    #[test]
    fn files_are_read_with_clear_errors() {
        let dir = std::env::temp_dir().join(format!("cloudmail-attach-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.md");
        std::fs::write(&file, b"# hi").unwrap();
        let a = OutgoingAttachment::from_path(&file).unwrap();
        assert_eq!(
            (a.filename.as_str(), a.mime_type.as_str(), a.content.as_slice()),
            ("notes.md", "text/markdown", &b"# hi"[..])
        );
        let missing = OutgoingAttachment::from_path(&dir.join("nope.pdf")).unwrap_err();
        assert!(missing.message.ends_with("nope.pdf: no such file"), "{}", missing.message);
        assert_eq!(missing.kind, ErrorKind::BadRequest);
        assert!(OutgoingAttachment::from_path(&dir).unwrap_err().message.ends_with("it's a directory"));
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn limits_count_every_file() {
        let a = OutgoingAttachment { content: vec![0; 600], ..Default::default() };
        assert!(check_limit(std::slice::from_ref(&a), 1000, "x").is_ok());
        let e = check_limit(&[a.clone(), a], 1000, "Cloudmail").unwrap_err();
        assert_eq!(
            e.message,
            "the attachments are too large to send through Cloudmail: 1 KB in all, and it takes at most 1000 B"
        );
    }
}
