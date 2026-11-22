//! The desktop app's name to the desktop. Until 0.8 it was `com.ferdousbhai.Cloudmail` (its `.desktop` file, icon and
//! window class); it is now Omarchy's one name, `cloudmail`, like the command. Someone who made it their mail app
//! (`xdg-mime default com.ferdousbhai.Cloudmail.desktop x-scheme-handler/mailto`) has that choice in a
//! `mimeapps.list`, which would name a file that no longer exists: [`migrate_mimeapps`] points it at the new one, once,
//! and touches nothing else there.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The `.desktop` file the app was installed as before 0.8.
pub const OLD_DESKTOP_FILE: &str = "com.ferdousbhai.Cloudmail.desktop";
/// The `.desktop` file it is installed as now.
pub const DESKTOP_FILE: &str = "cloudmail.desktop";

/// The `mimeapps.list` files a choice of default application may be in: the user's own, and the older place under
/// the data directory that some tools still write.
fn mimeapps_files() -> Vec<PathBuf> {
    [
        dirs::config_dir().map(|dir| dir.join("mimeapps.list")),
        dirs::data_dir().map(|dir| dir.join("applications").join("mimeapps.list")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Renames the old `.desktop` file to the new one in the user's `mimeapps.list` files, and answers which it changed.
/// A file without the old name is not written, so after the first run this only reads.
pub fn migrate_mimeapps() -> Vec<PathBuf> {
    mimeapps_files()
        .into_iter()
        .filter(|path| migrate_file(path).unwrap_or(false))
        .collect()
}

/// Renames the old `.desktop` file in one `mimeapps.list`, replacing it in one step (a temporary file beside it,
/// with its permissions, moved into place), so that a crash leaves the old file whole. Answers whether it changed.
pub fn migrate_file(path: &Path) -> io::Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let Some(renamed) = rename_entries(&text) else {
        return Ok(false);
    };
    let tmp = path.with_file_name(format!(".mimeapps.list.cloudmail-{}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.set_permissions(std::fs::metadata(path)?.permissions())?;
        file.write_all(renamed.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map(|()| true)
}

/// `text` with the old `.desktop` file named as the new one in each list of applications (`key=a.desktop;b.desktop;`),
/// the new name kept once where both were there; or `None` when the old name is in none of them. Group headers,
/// comments and every other entry stay as they are.
pub fn rename_entries(text: &str) -> Option<String> {
    let mut changed = false;
    let lines: Vec<String> = text
        .split_inclusive('\n')
        .map(|line| {
            let body = line.trim_end_matches(['\n', '\r']);
            let ending = &line[body.len()..];
            let Some((key, values)) = body.split_once('=') else {
                return line.to_owned();
            };
            if body.trim_start().starts_with('#') || !values.split(';').any(|v| v.trim() == OLD_DESKTOP_FILE) {
                return line.to_owned();
            }
            changed = true;
            let mut seen: Vec<&str> = Vec::new();
            for value in values.split(';').map(str::trim).filter(|v| !v.is_empty()) {
                let value = if value == OLD_DESKTOP_FILE { DESKTOP_FILE } else { value };
                if !seen.contains(&value) {
                    seen.push(value);
                }
            }
            let trailing = if values.trim_end().ends_with(';') { ";" } else { "" };
            format!("{key}={}{trailing}{ending}", seen.join(";"))
        })
        .collect();
    changed.then(|| lines.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_desktop_file_is_renamed_where_it_is_a_default() {
        let before = "[Default Applications]\n\
                      x-scheme-handler/mailto=com.ferdousbhai.Cloudmail.desktop;\n\
                      text/html=firefox.desktop\n\
                      # com.ferdousbhai.Cloudmail.desktop in a comment stays\n\
                      \n\
                      [Added Associations]\n\
                      x-scheme-handler/mailto=com.ferdousbhai.Cloudmail.desktop;cloudmail.desktop;thunderbird.desktop;\n";
        let after = rename_entries(before).unwrap();
        assert_eq!(
            after,
            "[Default Applications]\n\
             x-scheme-handler/mailto=cloudmail.desktop;\n\
             text/html=firefox.desktop\n\
             # com.ferdousbhai.Cloudmail.desktop in a comment stays\n\
             \n\
             [Added Associations]\n\
             x-scheme-handler/mailto=cloudmail.desktop;thunderbird.desktop;\n"
        );
        assert_eq!(rename_entries(&after), None, "once is enough");
        assert_eq!(
            rename_entries("[Default Applications]\ntext/html=firefox.desktop"),
            None
        );
        // A name that only starts like it is someone else's.
        assert_eq!(rename_entries("a=com.ferdousbhai.Cloudmail.desktop.bak;"), None);
    }

    #[test]
    fn a_mimeapps_list_is_replaced_whole_with_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cloudmail-desktop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mimeapps.list");
        std::fs::write(
            &path,
            "[Default Applications]\nx-scheme-handler/mailto=com.ferdousbhai.Cloudmail.desktop;\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(migrate_file(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[Default Applications]\nx-scheme-handler/mailto=cloudmail.desktop;\n"
        );
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o640);
        assert!(!migrate_file(&path).unwrap(), "nothing left to rename");
        assert!(!migrate_file(&dir.join("missing.list")).unwrap());
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["mimeapps.list"], "no temporary file left");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
