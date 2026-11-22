//! Messages being written, kept on this computer so that one is not lost when the app ends under it (a crash, a
//! logout, the power) or when its window is closed with "Save draft". Each is a JSON file of its own under the
//! state directory, written while its compose window has something worth keeping, removed when the message is sent
//! or discarded, and opened again in a compose window the next time the app starts. Attached files are not kept:
//! they are still where they were attached from.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::files;
use crate::util::Draft;

/// The directory the drafts are in.
#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The user's own (`$XDG_STATE_HOME/cloudmail/drafts`), or none when there is no state directory to keep it in.
    pub fn open() -> Option<Self> {
        Some(Self::at(dirs::state_dir()?.join("cloudmail").join("drafts")))
    }

    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Keeps `draft` as `id`, in place of what was kept under it before.
    pub fn save(&self, id: &str, draft: &Draft) -> io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        // Drafts are mail: the directory, like each file in it, is the user's alone.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let json = serde_json::json!({
            "to": draft.to,
            "cc": draft.cc,
            "subject": draft.subject,
            "body": draft.body,
            "from": draft.from,
            "reply_to_message_id": draft.reply_to_message_id,
        });
        files::replace_private(&self.path(id), json.to_string().as_bytes())
    }

    /// Forgets the draft kept as `id`; one that was never kept is not an error.
    pub fn remove(&self, id: &str) -> io::Result<()> {
        match std::fs::remove_file(self.path(id)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Every draft kept, oldest first, each with the `id` it is kept under. A file that is not a draft (a write
    /// that never finished, something else's) is left where it is and skipped.
    pub fn load(&self) -> Vec<Draft> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .filter_map(|e| e.ok()?.file_name().to_str()?.strip_suffix(".json").map(str::to_owned))
            .collect();
        // An id starts with the time it was made (`new_id`), so names in order are drafts in order.
        ids.sort();
        ids.into_iter()
            .filter_map(|id| {
                let json: serde_json::Value = serde_json::from_slice(&std::fs::read(self.path(&id)).ok()?).ok()?;
                let text = |key: &str| json.get(key).and_then(|v| v.as_str()).map(str::to_owned);
                Some(Draft {
                    to: text("to")?,
                    cc: text("cc")?,
                    subject: text("subject")?,
                    body: text("body")?,
                    from: text("from"),
                    reply_to_message_id: text("reply_to_message_id"),
                    id: Some(id),
                })
            })
            .collect()
    }
}

/// A name for a new draft: the time, so that drafts sort oldest first, then this process and a count, so that two
/// compose windows opened in the same instant still differ.
pub fn new_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:012}-{:09}-{}-{}",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::TestDir;

    fn draft(to: &str, body: &str) -> Draft {
        Draft {
            to: to.into(),
            body: body.into(),
            subject: "Lake photos".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_saved_draft_comes_back_until_it_is_removed() {
        let dir = TestDir::new("drafts");
        let store = Store::at(dir.0.join("cloudmail").join("drafts"));
        assert!(store.load().is_empty(), "no directory yet");
        let (first, second) = (new_id(), new_id());
        assert!(first < second, "ids sort in the order they were made");
        let reply = Draft {
            reply_to_message_id: Some("m2".into()),
            from: Some("billing@example.org".into()),
            ..draft("sam@example.com", "Line one\n\n> quoted “text”")
        };
        store.save(&second, &draft("ines@example.net", "later")).unwrap();
        store.save(&first, &reply).unwrap();
        // Saving again under the same id replaces the draft rather than adding one.
        store
            .save(&second, &draft("ines@example.net", "later, edited"))
            .unwrap();

        let loaded = store.load();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id.as_deref(), Some(first.as_str()));
        assert_eq!(loaded[0].to, "sam@example.com");
        assert_eq!(loaded[0].body, "Line one\n\n> quoted “text”");
        assert_eq!(loaded[0].subject, "Lake photos");
        assert_eq!(loaded[0].reply_to_message_id.as_deref(), Some("m2"));
        assert_eq!(loaded[0].from.as_deref(), Some("billing@example.org"));
        assert_eq!(loaded[1].body, "later, edited");
        assert_eq!(loaded[1].reply_to_message_id, None);

        store.remove(&first).unwrap();
        store.remove(&first).unwrap();
        assert_eq!(store.load().len(), 1);
        store.remove(&second).unwrap();
        assert!(store.load().is_empty());
    }

    #[test]
    fn drafts_are_the_user_s_alone_and_other_files_are_left() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("private");
        let store = Store::at(dir.0.join("drafts"));
        store.save("a", &draft("a@b.c", "hi")).unwrap();
        let mode = |p: PathBuf| p.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.0.join("drafts")), 0o700);
        assert_eq!(mode(dir.0.join("drafts").join("a.json")), 0o600);

        std::fs::write(dir.0.join("drafts").join("b.json"), b"{\"to\": \"cut off").unwrap();
        std::fs::write(dir.0.join("drafts").join("notes.txt"), b"not a draft").unwrap();
        assert_eq!(store.load().len(), 1, "only the whole draft");
        assert!(dir.0.join("drafts").join("b.json").exists(), "skipped, not deleted");
    }
}
