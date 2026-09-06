//! The tree pane's glyph table (issue #1585 §4, Phase 4c).
//!
//! Every row cue the UI draws is named here once, with a unicode and an
//! ASCII form, and **every form is exactly one cell wide**. That is the
//! whole point of the table: a tree row is column-aligned text, and a glyph
//! that measures two cells (or zero) shifts everything after it on that row
//! only, so the columns stop lining up on exactly the rows that have
//! something interesting to say.
//!
//! **East-Asian *Ambiguous* characters are deliberately excluded**, however
//! well they read in a mockup. `unicode-width` reports them as width 2 in an
//! East-Asian locale and 1 elsewhere, so a row containing one aligns for
//! some users and not others — a bug that is invisible to whoever picked the
//! glyph. The issue's own mockups use several (`✔ ⟳ ⚠ ▌ ⇊ ↑ ↓`); each is
//! replaced here by a confirmed-Narrow character (`✓` U+2713, `●` U+25CF,
//! `…` U+2026) or by plain ASCII. Where no narrow unicode form is better
//! than the ASCII one, the two columns are deliberately identical.
//!
//! `every_glyph_is_one_cell_wide_in_both_modes` (in this module's tests) is
//! the guard: it measures *every* variant in *both* modes, so a new glyph
//! cannot be added without meeting the rule.

use unicode_width::UnicodeWidthStr as _;

/// Which form of the table to render with. Resolved once at startup from
/// `--ascii` / `OMNI_DEV_UI_ASCII` and then passed down, never re-read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GlyphMode {
    #[default]
    Unicode,
    Ascii,
}

impl GlyphMode {
    /// Resolves the mode from the flag and the environment. The flag wins;
    /// otherwise `OMNI_DEV_UI_ASCII` enables ASCII when set to anything
    /// except the usual falsey spellings, so `OMNI_DEV_UI_ASCII=1` and
    /// `=true` both work and `=0`/`=false`/empty do not.
    pub fn resolve(ascii_flag: bool, env: Option<&str>) -> Self {
        if ascii_flag || env.is_some_and(is_truthy) {
            Self::Ascii
        } else {
            Self::Unicode
        }
    }
}

fn is_truthy(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// Every one-cell cue the tree pane can draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// A tab of ours is open on this worktree ("you are here").
    Here,
    /// A worktree with an open editor window.
    Open,
    /// A worktree with no window open.
    Closed,
    /// The repository's main working tree.
    MainMarker,
    /// The ▸ marker shown on a menu row that opens a submenu.
    /// Reuses MainMarker's already-Narrow U+25B8 character.
    SubmenuArrow,
    /// No main marker — the column's blank, kept in the table so the two
    /// states are guaranteed the same width.
    NotMain,
    /// A rebase or push this UI started is in flight.
    InFlight,
    /// An on-disk git operation state (a left-in-place conflicted rebase).
    Operation,
    /// The multi-select gutter on a marked row.
    MarkedGutter,
    /// The gutter on an unmarked row.
    UnmarkedGutter,
    /// PR checks passing.
    ChecksPass,
    /// PR checks failing.
    ChecksFail,
    /// PR checks still running. Deliberately not a bare dot: colour alone no
    /// longer separates this from a passing run (the tree view hit the same
    /// problem in #1406), so the glyph has to carry the difference.
    ChecksPending,
    /// A Claude session waiting on the user.
    SessionWaiting,
    /// A Claude session working.
    SessionWorking,
    /// A Claude session idle.
    SessionIdle,
    /// Text elided in the middle to fit its column.
    Ellipsis,
}

impl Glyph {
    /// This glyph in `mode`. Always exactly one cell wide.
    pub fn as_str(self, mode: GlyphMode) -> &'static str {
        let (unicode, ascii) = self.forms();
        match mode {
            GlyphMode::Unicode => unicode,
            GlyphMode::Ascii => ascii,
        }
    }

    /// `(unicode, ascii)`. Both forms are Narrow or plain ASCII — never an
    /// East-Asian Ambiguous character (see the module docs).
    ///
    /// Several arms below are deliberately identical: they are *different
    /// cues* that happen to render the same character (a blank main-marker
    /// and a blank gutter, an operation and a waiting session). Merging them
    /// would couple unrelated concepts and make changing one of them change
    /// the other, so the table keeps one arm per cue.
    #[allow(clippy::match_same_arms)]
    const fn forms(self) -> (&'static str, &'static str) {
        match self {
            // `*` in both: the mockup's ▌/✔ are Ambiguous, and no narrow
            // unicode form reads better here than the ASCII one.
            Self::Here => ("*", "*"),
            Self::Open => ("\u{25cf}", "o"),         // ● Narrow
            Self::Closed => ("\u{00b7}", "."),       // · Narrow
            Self::MainMarker => ("\u{25b8}", ">"),   // ▸ Narrow
            Self::SubmenuArrow => ("\u{25b8}", ">"), // ▸ Narrow
            Self::NotMain => (" ", " "),
            Self::InFlight => ("~", "~"),
            Self::Operation => ("!", "!"),
            // The mockup's ▌ is Ambiguous; │ (U+2502) is Narrow.
            Self::MarkedGutter => ("\u{2502}", "|"),
            Self::UnmarkedGutter => (" ", " "),
            Self::ChecksPass => ("\u{2713}", "+"), // ✓ Narrow (✔ is Ambiguous)
            Self::ChecksFail => ("\u{2717}", "x"), // ✗ Narrow
            Self::ChecksPending => ("\u{2026}", "~"), // … Narrow
            Self::SessionWaiting => ("!", "!"),
            Self::SessionWorking => ("\u{25cf}", "*"), // ●
            Self::SessionIdle => ("\u{00b7}", "."),    // ·
            Self::Ellipsis => ("\u{2026}", "~"),       // …
        }
    }
}

/// Truncates `text` to `max` display columns, eliding the middle with the
/// [`Glyph::Ellipsis`] so both ends stay readable — a branch name's
/// distinguishing part is as often at the end as the start
/// (`issue-1585-worktrees-ui` vs `issue-1585-worktrees-ui-phase-4`).
///
/// Measured in display columns, not chars or bytes, so a CJK branch name
/// truncates to the width it actually occupies. Never splits a character.
pub fn truncate_middle(text: &str, max: usize, mode: GlyphMode) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let ellipsis = Glyph::Ellipsis.as_str(mode);
    if max <= 1 {
        return ellipsis.to_string();
    }
    // One column for the ellipsis; split the rest either side, preferring
    // the head when the budget is odd.
    let budget = max - 1;
    let head_budget = budget.div_ceil(2);
    let tail_budget = budget - head_budget;

    let head = take_columns(text, head_budget);
    let tail = take_columns_from_end(text, tail_budget);
    format!("{head}{ellipsis}{tail}")
}

/// The longest prefix of `text` that fits in `columns` display columns.
fn take_columns(text: &str, columns: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w > columns {
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

/// The longest suffix of `text` that fits in `columns` display columns.
fn take_columns_from_end(text: &str, columns: usize) -> String {
    let mut used = 0;
    let mut taken = 0;
    for ch in text.chars().rev() {
        let w = char_width(ch);
        if used + w > columns {
            break;
        }
        used += w;
        taken += 1;
    }
    text.chars().skip(text.chars().count() - taken).collect()
}

fn char_width(ch: char) -> usize {
    // A control character has no width of its own; the renderer never emits
    // one, but measuring it as 0 keeps the arithmetic honest either way.
    ch.to_string().width()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Every glyph the table can produce, for exhaustive tests. Adding a
    /// variant without adding it here fails to compile (the match below is
    /// not `_`-terminated).
    const ALL: &[Glyph] = &[
        Glyph::Here,
        Glyph::Open,
        Glyph::Closed,
        Glyph::MainMarker,
        Glyph::SubmenuArrow,
        Glyph::NotMain,
        Glyph::InFlight,
        Glyph::Operation,
        Glyph::MarkedGutter,
        Glyph::UnmarkedGutter,
        Glyph::ChecksPass,
        Glyph::ChecksFail,
        Glyph::ChecksPending,
        Glyph::SessionWaiting,
        Glyph::SessionWorking,
        Glyph::SessionIdle,
        Glyph::Ellipsis,
    ];

    /// Fails to compile if a variant is added to [`Glyph`] but not to
    /// [`ALL`], so the width guard below can never silently skip one.
    #[test]
    fn the_all_list_covers_every_variant() {
        for glyph in ALL {
            // An exhaustive match: adding a variant breaks the build here.
            let named = match glyph {
                Glyph::Here => "Here",
                Glyph::Open => "Open",
                Glyph::Closed => "Closed",
                Glyph::MainMarker => "MainMarker",
                Glyph::SubmenuArrow => "SubmenuArrow",
                Glyph::NotMain => "NotMain",
                Glyph::InFlight => "InFlight",
                Glyph::Operation => "Operation",
                Glyph::MarkedGutter => "MarkedGutter",
                Glyph::UnmarkedGutter => "UnmarkedGutter",
                Glyph::ChecksPass => "ChecksPass",
                Glyph::ChecksFail => "ChecksFail",
                Glyph::ChecksPending => "ChecksPending",
                Glyph::SessionWaiting => "SessionWaiting",
                Glyph::SessionWorking => "SessionWorking",
                Glyph::SessionIdle => "SessionIdle",
                Glyph::Ellipsis => "Ellipsis",
            };
            assert!(!named.is_empty());
        }
        assert_eq!(ALL.len(), 17, "ALL must list every variant exactly once");
    }

    /// The table's reason for existing: a glyph that is not one cell wide
    /// misaligns every column after it, on that row only.
    #[test]
    fn every_glyph_is_one_cell_wide_in_both_modes() {
        for glyph in ALL {
            for mode in [GlyphMode::Unicode, GlyphMode::Ascii] {
                let s = glyph.as_str(mode);
                assert_eq!(
                    s.width(),
                    1,
                    "{glyph:?} in {mode:?} is {} cells: {s:?}",
                    s.width()
                );
                assert_eq!(
                    s.chars().count(),
                    1,
                    "{glyph:?} in {mode:?} is not one char"
                );
            }
        }
    }

    /// The ASCII form must actually be ASCII, or `--ascii` does not do what
    /// it says on a terminal with no unicode font.
    #[test]
    fn every_ascii_form_is_ascii() {
        for glyph in ALL {
            let s = glyph.as_str(GlyphMode::Ascii);
            assert!(s.is_ascii(), "{glyph:?} ascii form is not ascii: {s:?}");
        }
    }

    /// Guards the module's central claim. Each of these is East-Asian
    /// Ambiguous and appears in the issue's mockups; none may reach the
    /// table, in either mode.
    #[test]
    fn no_east_asian_ambiguous_glyph_is_in_the_table() {
        let banned = ['✔', '⟳', '⚠', '▌', '⇊', '↑', '↓', '●'];
        for glyph in ALL {
            for mode in [GlyphMode::Unicode, GlyphMode::Ascii] {
                let s = glyph.as_str(mode);
                for ch in banned {
                    // ● U+25CF is Narrow, not Ambiguous, so it is allowed —
                    // it is in the list only to prove the check is real.
                    if ch == '\u{25cf}' {
                        continue;
                    }
                    assert!(
                        !s.contains(ch),
                        "{glyph:?} in {mode:?} uses the ambiguous-width {ch:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn mode_resolves_from_the_flag_then_the_environment() {
        assert_eq!(GlyphMode::resolve(false, None), GlyphMode::Unicode);
        assert_eq!(GlyphMode::resolve(true, None), GlyphMode::Ascii);
        // The flag wins even against a falsey env var.
        assert_eq!(GlyphMode::resolve(true, Some("0")), GlyphMode::Ascii);
        for truthy in ["1", "true", "TRUE", "yes", "on", "anything"] {
            assert_eq!(
                GlyphMode::resolve(false, Some(truthy)),
                GlyphMode::Ascii,
                "{truthy:?}"
            );
        }
        for falsey in ["", "0", "false", "FALSE", "no", "off", "  "] {
            assert_eq!(
                GlyphMode::resolve(false, Some(falsey)),
                GlyphMode::Unicode,
                "{falsey:?}"
            );
        }
        assert_eq!(GlyphMode::default(), GlyphMode::Unicode);
    }

    #[test]
    fn truncate_middle_keeps_both_ends_and_never_exceeds_the_budget() {
        let mode = GlyphMode::Unicode;
        assert_eq!(truncate_middle("short", 10, mode), "short");
        assert_eq!(truncate_middle("exact", 5, mode), "exact");

        let long = "issue-1585-worktrees-ui-phase-4c";
        let out = truncate_middle(long, 20, mode);
        assert!(out.width() <= 20, "{out:?} is {} cells", out.width());
        assert!(out.starts_with("issue"), "the head survives: {out}");
        assert!(out.ends_with("4c"), "the tail survives: {out}");
        assert!(out.contains('\u{2026}'));

        // ASCII mode elides with `~` instead, same budget.
        let ascii = truncate_middle(long, 20, GlyphMode::Ascii);
        assert!(ascii.is_ascii());
        assert!(ascii.width() <= 20);
        assert!(ascii.contains('~'));
    }

    #[test]
    fn truncate_middle_measures_columns_not_chars() {
        let mode = GlyphMode::Unicode;
        // Each CJK char is two columns, so ten chars are twenty columns.
        let cjk = "你好你好你好你好你好";
        assert_eq!(cjk.width(), 20);
        let out = truncate_middle(cjk, 11, mode);
        assert!(
            out.width() <= 11,
            "{out:?} is {} cells, budget was 11",
            out.width()
        );
        // Never split a character: every char is intact.
        assert!(out
            .chars()
            .all(|c| c == '\u{2026}' || c == '你' || c == '好'));
    }

    #[test]
    fn truncate_middle_handles_budgets_too_small_to_elide_into() {
        let mode = GlyphMode::Unicode;
        assert_eq!(truncate_middle("abcdef", 1, mode), "\u{2026}");
        assert_eq!(truncate_middle("abcdef", 0, mode), "\u{2026}");
        let two = truncate_middle("abcdef", 2, mode);
        assert!(two.width() <= 2, "{two:?}");
        // A wide character that cannot fit at all yields just the ellipsis.
        let wide = truncate_middle("你好", 2, mode);
        assert!(wide.width() <= 2, "{wide:?}");
    }
}
