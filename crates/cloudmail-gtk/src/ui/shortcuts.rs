use omui::tr;

/// Every key the app answers to, by section, as the shortcuts overlay lists them. README.md has the same keys in one
/// table, in this order, and a test below fails when the two differ. The keys' names stay as they are in every
/// language; what they do is translated, and asked for each time the overlay opens.
pub fn sections() -> Vec<(String, Vec<(&'static str, String)>)> {
    vec![
        (
            tr("Reading"),
            vec![
                ("j / k", tr("Next / previous")),
                ("Enter / o", tr("Focus the message (scroll with arrows/space)")),
                ("e", tr("Archive")),
                ("i", tr("Move to Inbox")),
                ("#", tr("Delete (a linked account's: to its Trash)")),
                ("u", tr("Toggle unread")),
                ("r", tr("Reply")),
                ("a", tr("Reply all")),
                ("c", tr("Compose")),
                ("y / n", tr("Screener: let in / block")),
                ("L", tr("Load remote images")),
                ("R", tr("Refresh")),
            ],
        ),
        (
            tr("Boxes and search"),
            vec![
                ("1 2 3 4", tr("Screener · Inbox · Archive · Sent")),
                ("5 6 7 8", tr("HEY: The Feed · Paper Trail · Set Aside · Reply Later")),
                ("/", tr("Search")),
                ("Esc", tr("Clear search")),
            ],
        ),
        (
            tr("Writing"),
            vec![
                ("Ctrl+Enter", tr("Send (in compose)")),
                (
                    "Esc",
                    tr("Close the draft (in compose; asks first when there is something to lose)"),
                ),
            ],
        ),
        (
            tr("General"),
            vec![
                ("Ctrl+W", tr("Close this window (a draft asks first)")),
                ("Ctrl+Q", tr("Quit (every draft asks first)")),
                (
                    "? / F1",
                    tr("These shortcuts (while typing: F1, and Ctrl+? in compose)"),
                ),
            ],
        ),
    ]
}

/// The overlay of [`sections`], for one window: the main window shows it on `?` and F1, a compose window, which is
/// typed into, on F1 and Ctrl+?.
pub fn install(window: &impl gtk::prelude::IsA<gtk::Window>, typed_into: bool) {
    let overlay = omui::ShortcutOverlay::new(|| {
        sections()
            .iter()
            .map(|(title, keys)| {
                let keys: Vec<(&str, &str)> = keys.iter().map(|(key, does)| (*key, does.as_str())).collect();
                omui::ShortcutSection::new(title, &keys)
            })
            .collect()
    });
    overlay.install(window, typed_into);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// README.md's "Desktop keys" table, row for row: change a key above and the failure prints the table to paste.
    #[test]
    fn the_readme_lists_every_shortcut() {
        let rows: String = sections()
            .iter()
            .flat_map(|(_, keys)| keys.iter())
            .map(|(key, what)| format!("| `{key}` | {what} |\n"))
            .collect();
        let table = format!("| Key | Does |\n|---|---|\n{rows}\n");
        let readme = include_str!("../../../../README.md");
        assert!(
            readme.contains(&table),
            "README.md's table of desktop keys is out of step with the shortcuts; it should be:\n\n{table}"
        );
    }
}
