//! Key routing and encoding for the embedded terminal (issue #1585 Phase 3,
//! the minimal slice of §4's key table).
//!
//! Focus decides who receives a key. When a terminal tab has focus every
//! key goes to the child *except* `Alt`-chords, which are chrome — so the
//! chrome needs no tmux-style prefix. When the tree has focus, keys are
//! commands (handled in `app.rs`); only the chords are recognised here.
//!
//! Encoding is legacy xterm: the emulator is configured with
//! `kitty_keyboard` off (see `terminal::pty`), so a child never negotiates
//! the kitty keyboard protocol and never expects `CSI u` sequences. Only
//! `TermMode::APP_CURSOR` (arrow/Home/End form) and
//! `TermMode::BRACKETED_PASTE` are consulted.

use alacritty_terminal::term::TermMode;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// An `Alt`-chord the chrome handles regardless of focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeKey {
    /// `alt-e` / `alt-h`: focus the tree.
    FocusTree,
    /// `alt-l`: focus the terminal pane.
    FocusTerminal,
    /// `alt-t`: open a shell tab in the cursor worktree.
    NewShellTab,
    /// `alt-⇧t`: open a Claude tab in the cursor worktree.
    NewClaudeTab,
    /// `alt-s`: open a shell tab in a new group below (a split).
    SplitShellTab,
    /// `alt-w`: close the tab.
    CloseTab,
    /// `alt-]`: next tab in the focused group.
    NextTab,
    /// `alt-[`: previous tab in the focused group.
    PrevTab,
    /// `alt-1`…`alt-9`: select that tab (0-based here).
    SelectTab(usize),
    /// `alt-↓`: focus the group below.
    NextGroup,
    /// `alt-↑`: focus the group above.
    PrevGroup,
    /// `alt-⇧↓`: move the active tab into the group below.
    MoveTabDown,
    /// `alt-⇧↑`: move the active tab into the group above.
    MoveTabUp,
    /// `alt-0`: reset every group to an equal share.
    ResetLayout,
    /// `alt-f`: search the focused tab's scrollback.
    Find,
    /// `alt-⇧c`: clear every row colour at once.
    ClearAllRowColors,
    /// `alt-c`: copy the terminal selection.
    Copy,
    /// `⇧PageUp`: scroll the terminal back a page.
    ScrollPageUp,
    /// `⇧PageDown`: scroll the terminal forward a page.
    ScrollPageDown,
}

/// Where one key press goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRoute {
    /// A chrome chord — handled by the app whatever has focus.
    Chrome(ChromeKey),
    /// Bytes for the focused child's stdin.
    Passthrough(Vec<u8>),
    /// A key with no encoding (lock keys, media keys) or a chord that is
    /// chrome-shaped but unbound.
    Ignore,
}

/// Routes `key` for a *terminal-focused* pane running in `mode`. Chrome
/// chords win; everything else is encoded for the child.
pub fn route(key: &KeyEvent, mode: TermMode) -> KeyRoute {
    if let Some(chrome) = chrome_key(key) {
        return KeyRoute::Chrome(chrome);
    }
    match encode_key(key, mode) {
        Some(bytes) => KeyRoute::Passthrough(bytes),
        None => KeyRoute::Ignore,
    }
}

/// The chrome chord `key` is, if any — recognised in every focus state.
pub fn chrome_key(key: &KeyEvent) -> Option<ChromeKey> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('e' | 'h') if alt => Some(ChromeKey::FocusTree),
        KeyCode::Char('l') if alt => Some(ChromeKey::FocusTerminal),
        // Shift-t arrives as `Char('T')`, with or without the SHIFT bit.
        KeyCode::Char('T') if alt => Some(ChromeKey::NewClaudeTab),
        KeyCode::Char('t') if alt && shift => Some(ChromeKey::NewClaudeTab),
        KeyCode::Char('t') if alt => Some(ChromeKey::NewShellTab),
        KeyCode::Char('s') if alt => Some(ChromeKey::SplitShellTab),
        KeyCode::Char('w') if alt => Some(ChromeKey::CloseTab),
        // Shift-c arrives as `Char('C')`, with or without the SHIFT bit —
        // the same pair `alt-t`/`alt-⇧t` handles above.
        KeyCode::Char('C') if alt => Some(ChromeKey::ClearAllRowColors),
        KeyCode::Char('c') if alt && shift => Some(ChromeKey::ClearAllRowColors),
        KeyCode::Char('c') if alt => Some(ChromeKey::Copy),
        KeyCode::Char('f') if alt => Some(ChromeKey::Find),
        KeyCode::Char(']') if alt => Some(ChromeKey::NextTab),
        KeyCode::Char('[') if alt => Some(ChromeKey::PrevTab),
        KeyCode::Char('0') if alt => Some(ChromeKey::ResetLayout),
        // `alt-1`…`alt-9` select a tab; reported 0-based.
        KeyCode::Char(c @ '1'..='9') if alt => {
            Some(ChromeKey::SelectTab(c as usize - '1' as usize))
        }
        // Shift decides move-a-tab vs. move-focus, matching the tab strip's
        // own drag/reorder distinction.
        KeyCode::Up if alt && shift => Some(ChromeKey::MoveTabUp),
        KeyCode::Down if alt && shift => Some(ChromeKey::MoveTabDown),
        KeyCode::Up if alt => Some(ChromeKey::PrevGroup),
        KeyCode::Down if alt => Some(ChromeKey::NextGroup),
        KeyCode::PageUp if shift => Some(ChromeKey::ScrollPageUp),
        KeyCode::PageDown if shift => Some(ChromeKey::ScrollPageDown),
        _ => None,
    }
}

/// Legacy xterm encoding of `key` for a child running in `mode`.
pub fn encode_key(key: &KeyEvent, mode: TermMode) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let shift = mods.contains(KeyModifiers::SHIFT);
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let param = modifier_param(shift, alt, ctrl);

    let bytes: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            let mut out = Vec::with_capacity(5);
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                match control_byte(c) {
                    Some(b) => out.push(b),
                    None => out.extend(c.to_string().into_bytes()),
                }
            } else {
                out.extend(c.to_string().into_bytes());
            }
            out
        }
        KeyCode::Enter => prefixed(alt, b"\r"),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => prefixed(alt, if ctrl { b"\x08" } else { b"\x7f" }),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => cursor_key(b'A', app_cursor, param),
        KeyCode::Down => cursor_key(b'B', app_cursor, param),
        KeyCode::Right => cursor_key(b'C', app_cursor, param),
        KeyCode::Left => cursor_key(b'D', app_cursor, param),
        KeyCode::Home => cursor_key(b'H', app_cursor, param),
        KeyCode::End => cursor_key(b'F', app_cursor, param),
        KeyCode::Insert => tilde_key(2, param),
        KeyCode::Delete => tilde_key(3, param),
        KeyCode::PageUp => tilde_key(5, param),
        KeyCode::PageDown => tilde_key(6, param),
        KeyCode::F(n @ 1..=4) => {
            let letter = b'P' + (n - 1);
            match param {
                None => vec![0x1b, b'O', letter],
                Some(m) => format!("\x1b[1;{m}{}", letter as char).into_bytes(),
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            tilde_key(code, param)
        }
        KeyCode::F(_)
        | KeyCode::Null
        | KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::KeypadBegin
        | KeyCode::Media(_)
        | KeyCode::Modifier(_) => return None,
    };
    Some(bytes)
}

/// Bytes for pasted `text`: bracketed when the child asked for it,
/// otherwise with newlines normalised to `\r` the way a terminal would.
pub fn paste_bytes(text: &str, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// The xterm modifier parameter, or `None` when no modifier is held.
fn modifier_param(shift: bool, alt: bool, ctrl: bool) -> Option<u8> {
    let value = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    (value > 1).then_some(value)
}

fn control_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        ' ' | '@' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '/' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

fn prefixed(alt: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

fn cursor_key(letter: u8, app_cursor: bool, param: Option<u8>) -> Vec<u8> {
    match param {
        Some(m) => format!("\x1b[1;{m}{}", letter as char).into_bytes(),
        None if app_cursor => vec![0x1b, b'O', letter],
        None => vec![0x1b, b'[', letter],
    }
}

fn tilde_key(code: u8, param: Option<u8>) -> Vec<u8> {
    match param {
        Some(m) => format!("\x1b[{code};{m}~").into_bytes(),
        None => format!("\x1b[{code}~").into_bytes(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn alt_chords_are_chrome_in_every_focus_state() {
        let cases = [
            (KeyCode::Char('e'), KeyModifiers::ALT, ChromeKey::FocusTree),
            (
                KeyCode::Char('l'),
                KeyModifiers::ALT,
                ChromeKey::FocusTerminal,
            ),
            (
                KeyCode::Char('t'),
                KeyModifiers::ALT,
                ChromeKey::NewShellTab,
            ),
            (
                KeyCode::Char('T'),
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                ChromeKey::NewClaudeTab,
            ),
            (KeyCode::Char('w'), KeyModifiers::ALT, ChromeKey::CloseTab),
            (KeyCode::Char('c'), KeyModifiers::ALT, ChromeKey::Copy),
            (
                KeyCode::PageUp,
                KeyModifiers::SHIFT,
                ChromeKey::ScrollPageUp,
            ),
        ];
        for (code, mods, expected) in cases {
            assert_eq!(
                route(&key(code, mods), TermMode::default()),
                KeyRoute::Chrome(expected),
                "{code:?} {mods:?}"
            );
        }
    }

    #[test]
    fn plain_and_control_characters_pass_through_as_bytes() {
        let mode = TermMode::default();
        assert_eq!(
            route(&key(KeyCode::Char('a'), KeyModifiers::NONE), mode),
            KeyRoute::Passthrough(b"a".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), mode),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('x'), KeyModifiers::ALT), mode),
            Some(vec![0x1b, b'x'])
        );
        // A non-chord Esc goes to the child — vim/claude depend on it.
        assert_eq!(
            route(&key(KeyCode::Esc, KeyModifiers::NONE), mode),
            KeyRoute::Passthrough(vec![0x1b])
        );
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::NONE), mode),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::Backspace, KeyModifiers::NONE), mode),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn arrow_keys_follow_application_cursor_mode() {
        let normal = TermMode::default();
        let app = TermMode::default() | TermMode::APP_CURSOR;
        assert_eq!(
            encode_key(&key(KeyCode::Up, KeyModifiers::NONE), normal),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::Up, KeyModifiers::NONE), app),
            Some(b"\x1bOA".to_vec())
        );
        // A modifier forces the CSI form in either mode.
        assert_eq!(
            encode_key(&key(KeyCode::Up, KeyModifiers::SHIFT), app),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn tilde_and_function_keys_encode_with_modifier_params() {
        let mode = TermMode::default();
        assert_eq!(
            encode_key(&key(KeyCode::Delete, KeyModifiers::NONE), mode),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::PageDown, KeyModifiers::CONTROL), mode),
            Some(b"\x1b[6;5~".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::F(1), KeyModifiers::NONE), mode),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::F(5), KeyModifiers::NONE), mode),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::F(12), KeyModifiers::ALT), mode),
            Some(b"\x1b[24;3~".to_vec())
        );
    }

    #[test]
    fn keys_with_no_terminal_encoding_are_ignored() {
        assert_eq!(
            route(
                &key(KeyCode::CapsLock, KeyModifiers::NONE),
                TermMode::default()
            ),
            KeyRoute::Ignore
        );
    }

    #[test]
    fn every_navigation_and_editing_key_has_a_legacy_encoding() {
        let mode = TermMode::default();
        let none = KeyModifiers::NONE;
        let cases: &[(KeyCode, &[u8])] = &[
            (KeyCode::Tab, b"\t"),
            (KeyCode::BackTab, b"\x1b[Z"),
            (KeyCode::Down, b"\x1b[B"),
            (KeyCode::Right, b"\x1b[C"),
            (KeyCode::Left, b"\x1b[D"),
            (KeyCode::Home, b"\x1b[H"),
            (KeyCode::End, b"\x1b[F"),
            (KeyCode::Insert, b"\x1b[2~"),
            (KeyCode::PageUp, b"\x1b[5~"),
            (KeyCode::F(2), b"\x1bOQ"),
            (KeyCode::F(6), b"\x1b[17~"),
            (KeyCode::F(7), b"\x1b[18~"),
            (KeyCode::F(8), b"\x1b[19~"),
            (KeyCode::F(9), b"\x1b[20~"),
            (KeyCode::F(10), b"\x1b[21~"),
            (KeyCode::F(11), b"\x1b[23~"),
        ];
        for (code, expected) in cases {
            assert_eq!(
                encode_key(&key(*code, none), mode).as_deref(),
                Some(*expected),
                "{code:?}"
            );
        }
        // Home/End follow application-cursor mode like the arrows do.
        let app = mode | TermMode::APP_CURSOR;
        assert_eq!(
            encode_key(&key(KeyCode::Home, none), app),
            Some(b"\x1bOH".to_vec())
        );
        // A modified F1–F4 takes the CSI form.
        assert_eq!(
            encode_key(&key(KeyCode::F(1), KeyModifiers::CONTROL), mode),
            Some(b"\x1b[1;5P".to_vec())
        );
    }

    #[test]
    fn control_punctuation_maps_to_the_c0_bytes_and_alt_prefixes_enter() {
        let mode = TermMode::default();
        let ctrl = KeyModifiers::CONTROL;
        let cases: &[(char, u8)] = &[
            (' ', 0x00),
            ('@', 0x00),
            ('[', 0x1b),
            ('\\', 0x1c),
            (']', 0x1d),
            ('^', 0x1e),
            ('_', 0x1f),
            ('/', 0x1f),
            ('?', 0x7f),
            ('A', 0x01),
        ];
        for (c, byte) in cases {
            assert_eq!(
                encode_key(&key(KeyCode::Char(*c), ctrl), mode),
                Some(vec![*byte]),
                "ctrl-{c}"
            );
        }
        // A character with no control mapping passes through unchanged.
        assert_eq!(
            encode_key(&key(KeyCode::Char('1'), ctrl), mode),
            Some(b"1".to_vec())
        );
        // Alt prefixes Enter/Backspace with ESC.
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::ALT), mode),
            Some(b"\x1b\r".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::Backspace, KeyModifiers::CONTROL), mode),
            Some(vec![0x08])
        );
    }

    #[test]
    fn shift_page_down_is_a_scroll_chord() {
        assert_eq!(
            chrome_key(&key(KeyCode::PageDown, KeyModifiers::SHIFT)),
            Some(ChromeKey::ScrollPageDown)
        );
        assert_eq!(
            chrome_key(&key(KeyCode::PageDown, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            chrome_key(&key(
                KeyCode::Char('t'),
                KeyModifiers::ALT | KeyModifiers::SHIFT
            )),
            Some(ChromeKey::NewClaudeTab)
        );
    }

    #[test]
    fn paste_is_bracketed_only_when_the_child_asked_for_it() {
        assert_eq!(paste_bytes("a\nb", TermMode::default()), b"a\rb".to_vec());
        assert_eq!(
            paste_bytes("a\nb", TermMode::default() | TermMode::BRACKETED_PASTE),
            b"\x1b[200~a\nb\x1b[201~".to_vec()
        );
    }
}
