//! `cloudmail open`: the desktop app, which is a program of its own (`cloudmail-gtk`, installed under
//! `/usr/lib/cloudmail/`) so that this command line links nothing of GTK and runs where there is no display. The
//! launcher's entry, `cloudmail.desktop`, runs this.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use crate::cli::OpenArgs;
use crate::output::{CliError, CliResult};

/// The app's file name, wherever it is installed.
const APP: &str = "cloudmail-gtk";

/// Where the app is, for a `cloudmail` at `exe`: `$CLOUDMAIL_APP` when set (a test's stand-in, a build of one's
/// own), else beside this program's prefix in `lib/cloudmail/` (`/usr/bin` and `/usr/lib/cloudmail`, or `~/.local/bin`
/// and `~/.local/lib/cloudmail`), else in the same directory, as a `cargo build` leaves them.
fn find_app(exe: &Path, override_path: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = override_path {
        return Some(path);
    }
    let dir = exe.parent()?;
    let candidates = [
        dir.parent()
            .map(|prefix| prefix.join("lib").join("cloudmail").join(APP)),
        Some(dir.join(APP)),
    ];
    candidates.into_iter().flatten().find(|path| path.is_file())
}

/// Starts the desktop app in this process's place, handing it the `mailto:` links. It only returns when the app
/// could not be started.
pub fn open(args: &OpenArgs) -> CliResult {
    let exe =
        std::env::current_exe().map_err(|e| CliError::generic(format!("couldn't tell where cloudmail is: {e}")))?;
    let app = find_app(&exe, std::env::var_os("CLOUDMAIL_APP").map(PathBuf::from)).ok_or_else(|| {
        CliError::not_found("the desktop app isn't installed beside this cloudmail")
            .hint("install the cloudmail package, or run ./install.sh from a checkout without CLOUDMAIL_NO_GTK=1")
    })?;
    let error = std::process::Command::new(&app).args(&args.links).exec();
    Err(CliError::generic(format!("couldn't start {}: {error}", app.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_is_found_under_lib_then_beside_this_program() {
        let root = std::env::temp_dir().join(format!("cloudmail-open-{}", std::process::id()));
        let (bin, lib) = (root.join("bin"), root.join("lib").join("cloudmail"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        let exe = bin.join("cloudmail");
        assert_eq!(find_app(&exe, None), None, "not installed");
        std::fs::write(bin.join(APP), "").unwrap();
        assert_eq!(find_app(&exe, None), Some(bin.join(APP)), "a cargo build's");
        std::fs::write(lib.join(APP), "").unwrap();
        assert_eq!(find_app(&exe, None), Some(lib.join(APP)), "the package's comes first");
        assert_eq!(find_app(&exe, Some("/opt/x".into())), Some(PathBuf::from("/opt/x")));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
