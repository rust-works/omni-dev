//! Per-surface context-menu contents (issue #1602 Phase 3).
//!
//! Every builder here is a **pure function of small inputs** returning
//! [`MenuEntry`]s — no `App`, no `Frame`, no daemon. That is what lets the
//! per-surface contents be unit-tested directly ("the strip menu hides *Move
//! to Group Above* when there is no group above") instead of through a
//! rendered frame, and it keeps `app.rs` from growing a fourth menu's worth
//! of list-building.
//!
//! The tree-row menu is the one that is *not* built here: its contents come
//! from [`actions::applicable_action_groups`], because they must stay
//! identical to what the `a` key and the `:` palette offer.

use super::actions::{self, ActionKind, Target};
use super::keys::ChromeKey;
use super::popup::{MenuCommand, MenuEntry, MenuItem, UiAction};
use super::row_colors;

/// The tree-row menu: the applicable actions, grouped, plus the row-colour
/// submenu.
pub fn tree_menu(targets: &[Target]) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    for group in actions::applicable_action_groups(targets) {
        if !entries.is_empty() {
            entries.push(MenuEntry::Separator);
        }
        entries.extend(
            group
                .into_iter()
                .map(|(action, label)| MenuEntry::Item(MenuItem::action(action, label))),
        );
    }
    // Row Colour is its own group: neither a git action nor a close, and a
    // submenu keeps eleven colours from burying the rest of the menu.
    if !targets.is_empty() {
        if !entries.is_empty() {
            entries.push(MenuEntry::Separator);
        }
        entries.push(MenuEntry::Submenu {
            label: "Row Colour",
            items: row_color_items(),
        });
        entries.push(MenuEntry::Item(MenuItem::action(
            ActionKind::ClearRowColor,
            "Clear Row Colour",
        )));
    }
    entries
}

/// The row-colour palette as menu items — shared by the `c` picker and the
/// *Row Colour* submenu, so the two cannot list different colours.
pub fn row_color_items() -> Vec<MenuItem> {
    row_colors::KNOWN_ROW_COLORS
        .iter()
        .map(|&color| MenuItem::action(ActionKind::SetRowColor(color), color))
        .collect()
}

/// The tab-strip menu for tab `tab` of group `group`.
///
/// The move-to-group entries are **hidden, not disabled**, when there is no
/// such group — matching how `applicable_actions` treats an inapplicable
/// action on a tree row, so the two menus behave the same way about things
/// that do not apply.
pub fn strip_menu(groups: usize, group: usize, tabs: usize, tab: usize) -> Vec<MenuEntry> {
    let mut entries = vec![MenuEntry::Item(MenuItem::new(
        MenuCommand::Chrome(ChromeKey::CloseTab),
        "Close Tab",
    ))];
    if tabs > 1 {
        entries.push(MenuEntry::Item(MenuItem::new(
            MenuCommand::Ui(UiAction::CloseOtherTabs),
            "Close Other Tabs",
        )));
    }
    if tab + 1 < tabs {
        entries.push(MenuEntry::Item(MenuItem::new(
            MenuCommand::Ui(UiAction::CloseTabsToRight),
            "Close Tabs to the Right",
        )));
    }

    let mut moves = Vec::new();
    if group > 0 {
        moves.push(MenuEntry::Item(MenuItem::new(
            MenuCommand::Chrome(ChromeKey::MoveTabUp),
            "Move to Group Above",
        )));
    }
    if group + 1 < groups {
        moves.push(MenuEntry::Item(MenuItem::new(
            MenuCommand::Chrome(ChromeKey::MoveTabDown),
            "Move to Group Below",
        )));
    }
    if !moves.is_empty() {
        entries.push(MenuEntry::Separator);
        entries.extend(moves);
    }

    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Item(MenuItem::new(
        MenuCommand::Chrome(ChromeKey::SplitShellTab),
        "Split Below",
    )));
    entries.push(MenuEntry::Item(MenuItem::new(
        MenuCommand::Chrome(ChromeKey::NewShellTab),
        "New Shell Tab",
    )));
    entries.push(MenuEntry::Item(MenuItem::new(
        MenuCommand::Chrome(ChromeKey::NewClaudeTab),
        "New Claude Tab",
    )));
    entries
}

/// The terminal-grid menu.
///
/// *Copy* is the one entry that is **disabled rather than hidden**: it is the
/// reason to open this menu at all, so hiding it with no selection would read
/// as the menu being broken. Everything else here always applies.
///
/// **Paste is deliberately absent.** `clipboard.rs` only ever writes; adding a
/// read would create a clipboard→PTY byte path that does not exist today, and
/// that is a capability change wanting its own decision (#1602 *Out of
/// scope*). A grep guard keeps it that way.
pub fn grid_menu(has_selection: bool) -> Vec<MenuEntry> {
    vec![
        MenuEntry::Item(MenuItem {
            command: MenuCommand::Chrome(ChromeKey::Copy),
            label: "Copy",
            enabled: has_selection,
        }),
        MenuEntry::Item(MenuItem::new(
            MenuCommand::Ui(UiAction::SelectAll),
            "Select All",
        )),
        MenuEntry::Item(MenuItem {
            command: MenuCommand::Ui(UiAction::ClearSelection),
            label: "Clear Selection",
            enabled: has_selection,
        }),
        MenuEntry::Separator,
        MenuEntry::Item(MenuItem::new(
            MenuCommand::Chrome(ChromeKey::Find),
            "Find in Scrollback",
        )),
        MenuEntry::Item(MenuItem::new(
            MenuCommand::Ui(UiAction::ScrollToBottom),
            "Scroll to Bottom",
        )),
        MenuEntry::Separator,
        MenuEntry::Item(MenuItem::new(
            MenuCommand::Chrome(ChromeKey::CloseTab),
            "Close Tab",
        )),
    ]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn labels(entries: &[MenuEntry]) -> Vec<&'static str> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Item(item) => Some(item.label),
                MenuEntry::Separator => None,
                MenuEntry::Submenu { label, .. } => Some(label),
            })
            .collect()
    }

    fn item<'a>(entries: &'a [MenuEntry], label: &str) -> Option<&'a MenuItem> {
        entries.iter().find_map(|entry| match entry {
            MenuEntry::Item(item) if item.label == label => Some(item),
            _ => None,
        })
    }

    /// No menu ever begins or ends with a separator, and no two separators
    /// are ever adjacent — the classic bug when entries are conditionally
    /// hidden and the separator around them is not.
    fn assert_no_stray_separators(entries: &[MenuEntry]) {
        assert!(
            !matches!(entries.first(), Some(MenuEntry::Separator)),
            "menu starts with a separator: {entries:?}"
        );
        assert!(
            !matches!(entries.last(), Some(MenuEntry::Separator)),
            "menu ends with a separator: {entries:?}"
        );
        assert!(
            !entries
                .windows(2)
                .any(|pair| matches!(pair, [MenuEntry::Separator, MenuEntry::Separator])),
            "menu has two adjacent separators: {entries:?}"
        );
    }

    #[test]
    fn strip_menu_hides_move_up_only_at_the_first_group() {
        let entries = strip_menu(3, 0, 2, 0);
        assert!(!labels(&entries).contains(&"Move to Group Above"));
        assert!(labels(&entries).contains(&"Move to Group Below"));
    }

    #[test]
    fn strip_menu_hides_move_down_only_at_the_last_group() {
        let entries = strip_menu(3, 2, 2, 0);
        assert!(labels(&entries).contains(&"Move to Group Above"));
        assert!(!labels(&entries).contains(&"Move to Group Below"));
    }

    #[test]
    fn strip_menu_shows_both_moves_for_a_middle_group() {
        let entries = strip_menu(3, 1, 2, 0);
        assert!(labels(&entries).contains(&"Move to Group Above"));
        assert!(labels(&entries).contains(&"Move to Group Below"));
    }

    #[test]
    fn strip_menu_hides_close_other_tabs_with_only_one_tab() {
        let entries = strip_menu(1, 0, 1, 0);
        assert!(!labels(&entries).contains(&"Close Other Tabs"));
    }

    #[test]
    fn strip_menu_shows_close_other_tabs_with_more_than_one_tab() {
        let entries = strip_menu(1, 0, 3, 1);
        assert!(labels(&entries).contains(&"Close Other Tabs"));
    }

    #[test]
    fn strip_menu_hides_close_to_the_right_on_the_last_tab() {
        let entries = strip_menu(1, 0, 3, 2);
        assert!(!labels(&entries).contains(&"Close Tabs to the Right"));
    }

    #[test]
    fn strip_menu_shows_close_to_the_right_off_the_last_tab() {
        let entries = strip_menu(1, 0, 3, 0);
        assert!(labels(&entries).contains(&"Close Tabs to the Right"));
    }

    #[test]
    fn grid_menu_copy_is_disabled_with_no_selection_and_enabled_with_one() {
        let no_selection = grid_menu(false);
        assert!(!item(&no_selection, "Copy").expect("a Copy entry").enabled);
        assert!(
            item(&no_selection, "Select All")
                .expect("a Select All entry")
                .enabled,
            "Select All is always enabled"
        );

        let with_selection = grid_menu(true);
        assert!(item(&with_selection, "Copy").expect("a Copy entry").enabled);
        assert!(
            item(&with_selection, "Select All")
                .expect("a Select All entry")
                .enabled
        );
    }

    /// #1602 Out of scope: `clipboard.rs` only ever writes, so a Paste entry
    /// would open a clipboard-to-PTY byte path that does not exist. A grep
    /// guard keeps the label out of the source; this pins the same rule at
    /// the level of the built menu.
    #[test]
    fn grid_menu_has_no_paste_entry() {
        let entries = grid_menu(true);
        assert!(
            labels(&entries)
                .iter()
                .all(|label| !label.contains("Paste")),
            "grid menu must never offer Paste: {entries:?}"
        );
    }

    /// `labels()` also has to see past the tree menu's one non-`Item` entry
    /// that isn't a separator: the *Row Colour* submenu.
    #[test]
    fn labels_reports_a_submenus_own_label() {
        let entries = tree_menu(&[Target::Worktree {
            path: std::path::PathBuf::from("/repo/wt"),
            is_main: false,
            github: None,
            pr_url: None,
            branch: None,
            sessions: Vec::new(),
        }]);
        assert!(labels(&entries).contains(&"Row Colour"));
    }

    #[test]
    fn no_menu_shape_has_stray_separators() {
        assert_no_stray_separators(&strip_menu(1, 0, 1, 0));
        assert_no_stray_separators(&strip_menu(3, 0, 1, 0));
        assert_no_stray_separators(&strip_menu(3, 2, 1, 0));
        assert_no_stray_separators(&strip_menu(3, 1, 3, 1));
        assert_no_stray_separators(&grid_menu(false));
        assert_no_stray_separators(&grid_menu(true));
        assert_no_stray_separators(&tree_menu(&[]));
        assert_no_stray_separators(&tree_menu(&[Target::Worktree {
            path: std::path::PathBuf::from("/repo/wt"),
            is_main: false,
            github: None,
            pr_url: None,
            branch: None,
            sessions: Vec::new(),
        }]));
    }
}
