//! Keystrokes in, document edits out.
//!
//! ## Why there is an `Action` in the middle
//!
//! A keymap that calls editor methods directly cannot be rebound, cannot be
//! recorded, and cannot be tested without a window. So a key event resolves to
//! an [`Action`] — a description of intent, not of a keystroke — and
//! [`apply`] carries that action out against a [`Document`] and a
//! [`Viewport`]. Rebinding replaces the first half; a macro recorder taps the
//! middle; a test drives the second half directly.
//!
//! ## Modifier conventions
//!
//! The primary modifier is <kbd>Ctrl</kbd> everywhere except macOS, where it is
//! <kbd>Cmd</kbd>. [`Modifiers::primary`] resolves that at compile time so the
//! keymap itself is written once, and word-wise motion uses <kbd>Alt</kbd> on
//! macOS and <kbd>Ctrl</kbd> elsewhere, matching every other editor on each
//! platform.

use nebula_core::{Document, Edit, Selection, SelectionSet, Transaction, word};

use crate::{Result, layout::Viewport};

/// A physical key, independent of any windowing library.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// A character-producing key, already mapped through the keyboard layout.
    Char(char),
    /// <kbd>Enter</kbd>.
    Enter,
    /// <kbd>Tab</kbd>.
    Tab,
    /// <kbd>Backspace</kbd>.
    Backspace,
    /// <kbd>Delete</kbd>.
    Delete,
    /// <kbd>Escape</kbd>.
    Escape,
    /// <kbd>←</kbd>.
    Left,
    /// <kbd>→</kbd>.
    Right,
    /// <kbd>↑</kbd>.
    Up,
    /// <kbd>↓</kbd>.
    Down,
    /// <kbd>Home</kbd>.
    Home,
    /// <kbd>End</kbd>.
    End,
    /// <kbd>Page Up</kbd>.
    PageUp,
    /// <kbd>Page Down</kbd>.
    PageDown,
    /// A function key, 1-based.
    Function(u8),
}

/// Which modifiers were held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    /// <kbd>Ctrl</kbd>.
    pub ctrl: bool,
    /// <kbd>Shift</kbd>.
    pub shift: bool,
    /// <kbd>Alt</kbd> / <kbd>Option</kbd>.
    pub alt: bool,
    /// <kbd>Cmd</kbd> / <kbd>Super</kbd> / <kbd>Win</kbd>.
    pub meta: bool,
}

impl Modifiers {
    /// Nothing held.
    pub const NONE: Modifiers = Modifiers { ctrl: false, shift: false, alt: false, meta: false };

    /// Only <kbd>Shift</kbd>.
    pub const SHIFT: Modifiers = Modifiers { shift: true, ..Modifiers::NONE };

    /// Only the platform's primary modifier.
    pub const PRIMARY: Modifiers = if cfg!(target_os = "macos") {
        Modifiers { meta: true, ..Modifiers::NONE }
    } else {
        Modifiers { ctrl: true, ..Modifiers::NONE }
    };

    /// The primary modifier plus <kbd>Shift</kbd>.
    pub const PRIMARY_SHIFT: Modifiers = Modifiers { shift: true, ..Modifiers::PRIMARY };

    /// The word-motion modifier: <kbd>Alt</kbd> on macOS, <kbd>Ctrl</kbd>
    /// elsewhere.
    pub const WORD: Modifiers = if cfg!(target_os = "macos") {
        Modifiers { alt: true, ..Modifiers::NONE }
    } else {
        Modifiers { ctrl: true, ..Modifiers::NONE }
    };

    /// Whether the platform's primary modifier is held.
    pub const fn primary(&self) -> bool {
        if cfg!(target_os = "macos") { self.meta } else { self.ctrl }
    }

    /// Whether the word-motion modifier is held.
    pub const fn word(&self) -> bool {
        if cfg!(target_os = "macos") { self.alt } else { self.ctrl }
    }

    /// Whether no modifier that changes meaning is held.
    ///
    /// <kbd>Shift</kbd> is excluded: it selects rather than rebinds, and on many
    /// layouts it is also how you type half the characters on the keyboard.
    pub const fn is_plain(&self) -> bool {
        !self.ctrl && !self.alt && !self.meta
    }
}

/// A key press.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    /// Which key.
    pub key: Key,
    /// Which modifiers.
    pub modifiers: Modifiers,
    /// Whether this press came from the OS key-repeat, rather than a fresh
    /// press. Undo grouping uses it: a held-down key is one gesture.
    pub repeat: bool,
}

impl KeyEvent {
    /// A press with no modifiers.
    pub fn new(key: Key) -> Self {
        Self { key, modifiers: Modifiers::NONE, repeat: false }
    }

    /// A press with modifiers.
    pub fn with(key: Key, modifiers: Modifiers) -> Self {
        Self { key, modifiers, repeat: false }
    }

    /// A typed character with no modifiers.
    pub fn char(c: char) -> Self {
        Self::new(Key::Char(c))
    }
}

/// Which way a motion goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Motion {
    /// One character left.
    Left,
    /// One character right.
    Right,
    /// One visual line up.
    Up,
    /// One visual line down.
    Down,
    /// To the previous word boundary.
    WordLeft,
    /// To the next word boundary.
    WordRight,
    /// To the first non-whitespace character, then to column 0.
    LineStart,
    /// To the end of the line.
    LineEnd,
    /// To offset 0.
    DocumentStart,
    /// To the end of the document.
    DocumentEnd,
    /// One screenful up.
    PageUp,
    /// One screenful down.
    PageDown,
}

/// What a keystroke means.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Insert literal text at every cursor.
    Insert(String),
    /// Insert a line break, carrying the current indentation.
    Newline,
    /// Insert an indent.
    Indent,
    /// Remove one level of indentation from the cursor's line.
    Outdent,
    /// Delete the character before each cursor, or the selection.
    DeleteBackward,
    /// Delete the character after each cursor, or the selection.
    DeleteForward,
    /// Delete to the previous word boundary.
    DeleteWordBackward,
    /// Delete to the next word boundary.
    DeleteWordForward,
    /// Delete from each cursor to the end of its line.
    DeleteToLineEnd,
    /// Move every cursor, collapsing selections.
    Move(Motion),
    /// Move every cursor's head, keeping its anchor.
    Extend(Motion),
    /// Select the whole document.
    SelectAll,
    /// Select the word under the primary cursor.
    SelectWord,
    /// Select the line the primary cursor is on.
    SelectLine,
    /// Collapse to a single cursor.
    CollapseSelection,
    /// Add a cursor one line above the topmost cursor.
    AddCursorAbove,
    /// Add a cursor one line below the bottommost cursor.
    AddCursorBelow,
    /// Undo one transaction group.
    Undo,
    /// Redo one transaction group.
    Redo,
    /// Scroll without moving the cursor.
    Scroll(isize),
    /// Put the primary cursor's line in the middle of the viewport.
    CenterCursor,
    /// Save the document.
    Save,
    /// Close the document.
    Close,
    /// Quit the editor.
    Quit,
    /// Open the command palette.
    CommandPalette,
    /// Open the file finder.
    FindFile,
    /// Open search.
    Search,
    /// Focus the AI panel.
    AiPanel,
}

impl Action {
    /// Whether this action changes the document's text.
    pub const fn is_edit(&self) -> bool {
        matches!(
            self,
            Action::Insert(_)
                | Action::Newline
                | Action::Indent
                | Action::Outdent
                | Action::DeleteBackward
                | Action::DeleteForward
                | Action::DeleteWordBackward
                | Action::DeleteWordForward
                | Action::DeleteToLineEnd
                | Action::Undo
                | Action::Redo
        )
    }

    /// Whether this action is handled by the editor shell rather than the
    /// document — the difference between "the buffer changed" and "a panel
    /// opened".
    pub const fn is_global(&self) -> bool {
        matches!(
            self,
            Action::Save
                | Action::Close
                | Action::Quit
                | Action::CommandPalette
                | Action::FindFile
                | Action::Search
                | Action::AiPanel
        )
    }
}

/// Resolve a key event to an action.
///
/// Returns `None` for keys with no binding, which the caller should ignore
/// rather than treat as an error — a stray <kbd>F13</kbd> is not a failure.
pub fn keymap(event: &KeyEvent) -> Option<Action> {
    let m = event.modifiers;

    match &event.key {
        // A character with the primary modifier is a command; a character
        // without one is text, including when Shift is held to capitalise it.
        Key::Char(c) if m.primary() && !m.alt => match c.to_ascii_lowercase() {
            's' => Some(Action::Save),
            'z' if m.shift => Some(Action::Redo),
            'z' => Some(Action::Undo),
            'y' if !cfg!(target_os = "macos") => Some(Action::Redo),
            'a' => Some(Action::SelectAll),
            'd' => Some(Action::SelectWord),
            'l' => Some(Action::SelectLine),
            'w' => Some(Action::Close),
            'q' => Some(Action::Quit),
            'p' if m.shift => Some(Action::CommandPalette),
            'p' => Some(Action::FindFile),
            'f' => Some(Action::Search),
            'k' => Some(Action::AiPanel),
            'g' => Some(Action::CenterCursor),
            _ => None,
        },

        Key::Char(c) if m.is_plain() => Some(Action::Insert(c.to_string())),
        Key::Char(_) => None,

        Key::Enter if m.is_plain() => Some(Action::Newline),
        Key::Enter => None,

        Key::Tab if m.shift => Some(Action::Outdent),
        Key::Tab if m.is_plain() => Some(Action::Indent),
        Key::Tab => None,

        Key::Backspace if m.word() => Some(Action::DeleteWordBackward),
        Key::Backspace => Some(Action::DeleteBackward),

        Key::Delete if m.word() => Some(Action::DeleteWordForward),
        Key::Delete if m.primary() && m.shift => Some(Action::DeleteToLineEnd),
        Key::Delete => Some(Action::DeleteForward),

        Key::Escape => Some(Action::CollapseSelection),

        Key::Left => Some(motion(m, if m.word() { Motion::WordLeft } else { Motion::Left })),
        Key::Right => Some(motion(m, if m.word() { Motion::WordRight } else { Motion::Right })),

        // Ctrl/Cmd with a vertical arrow adds a cursor rather than moving one:
        // this is how multi-cursor editing is reached without a mouse.
        Key::Up if m.primary() && m.alt => Some(Action::AddCursorAbove),
        Key::Down if m.primary() && m.alt => Some(Action::AddCursorBelow),
        Key::Up if m.primary() => Some(Action::Scroll(-1)),
        Key::Down if m.primary() => Some(Action::Scroll(1)),
        Key::Up => Some(motion(m, Motion::Up)),
        Key::Down => Some(motion(m, Motion::Down)),

        Key::Home if m.primary() => Some(motion(m, Motion::DocumentStart)),
        Key::Home => Some(motion(m, Motion::LineStart)),
        Key::End if m.primary() => Some(motion(m, Motion::DocumentEnd)),
        Key::End => Some(motion(m, Motion::LineEnd)),

        Key::PageUp => Some(motion(m, Motion::PageUp)),
        Key::PageDown => Some(motion(m, Motion::PageDown)),

        Key::Function(1) => Some(Action::CommandPalette),
        Key::Function(_) => None,
    }
}

/// Shift turns any motion into a selection.
fn motion(modifiers: Modifiers, motion: Motion) -> Action {
    if modifiers.shift { Action::Extend(motion) } else { Action::Move(motion) }
}

/// What happened when an action was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Applied {
    /// Whether the document's text changed.
    pub edited: bool,
    /// Whether the selection changed.
    pub moved: bool,
    /// Whether the viewport changed.
    pub scrolled: bool,
    /// Whether the action needs the shell rather than the document.
    pub global: bool,
}

impl Applied {
    /// Whether the screen has to be redrawn.
    pub const fn needs_redraw(&self) -> bool {
        self.edited || self.moved || self.scrolled
    }
}

/// How many spaces one indent level is.
///
/// A tab-vs-spaces setting belongs in per-project configuration; four spaces is
/// what the editor does until one says otherwise.
pub const INDENT_WIDTH: usize = 4;

/// Carry out an action.
pub fn apply(
    action: &Action,
    document: &mut Document,
    viewport: &mut Viewport,
    groupable: bool,
) -> Result<Applied> {
    let mut outcome = Applied::default();

    match action {
        Action::Insert(text) => {
            document.insert_at_cursors(text, groupable)?;
            outcome.edited = true;
        }

        Action::Newline => {
            // Auto-indent: a newline inherits the current line's leading
            // whitespace, because a language-aware indenter that gets it wrong
            // is more annoying than one that simply keeps the level.
            let offset = document.selections().primary().head;
            let line = document.buffer().offset_to_line(offset)?;
            let indent = word::line_indent(document.buffer(), line);
            document.insert_at_cursors(&format!("\n{indent}"), false)?;
            outcome.edited = true;
        }

        Action::Indent => {
            document.insert_at_cursors(&" ".repeat(INDENT_WIDTH), groupable)?;
            outcome.edited = true;
        }

        Action::Outdent => {
            outcome.edited = outdent(document)?;
        }

        Action::DeleteBackward => {
            document.delete_backward()?;
            outcome.edited = true;
        }

        Action::DeleteForward => {
            document.delete_forward()?;
            outcome.edited = true;
        }

        Action::DeleteWordBackward => {
            outcome.edited = delete_to(document, word::prev_word_boundary)?;
        }

        Action::DeleteWordForward => {
            outcome.edited = delete_to(document, word::next_word_boundary)?;
        }

        Action::DeleteToLineEnd => {
            outcome.edited = delete_to(document, |buffer, offset| {
                let line = buffer.offset_to_line(offset).unwrap_or(0);
                let end = buffer.line_end(line).unwrap_or(offset);
                // At the end of a line, take the newline itself, so repeated
                // presses join lines rather than doing nothing.
                if end == offset { (offset + 1).min(buffer.len_chars()) } else { end }
            })?;
        }

        Action::Move(motion) => {
            move_cursors(document, *motion, viewport, false)?;
            outcome.moved = true;
        }

        Action::Extend(motion) => {
            move_cursors(document, *motion, viewport, true)?;
            outcome.moved = true;
        }

        Action::SelectAll => {
            let end = document.buffer().len_chars();
            document.set_selections(SelectionSet::single(Selection::new(0, end)));
            outcome.moved = true;
        }

        Action::SelectWord => {
            let range = document.word_at_cursor();
            document.set_selections(SelectionSet::single(Selection::new(range.start, range.end)));
            outcome.moved = true;
        }

        Action::SelectLine => {
            let offset = document.selections().primary().head;
            let line = document.buffer().offset_to_line(offset)?;
            let start = document.buffer().line_start(line)?;
            // Include the newline, so that deleting the selection removes the
            // line rather than leaving an empty one behind.
            let end = (document.buffer().line_end(line)? + 1).min(document.buffer().len_chars());
            document.set_selections(SelectionSet::single(Selection::new(start, end)));
            outcome.moved = true;
        }

        Action::CollapseSelection => {
            let mut selections = document.selections().clone();
            selections.keep_primary_only();
            selections.collapse();
            document.set_selections(selections);
            outcome.moved = true;
        }

        Action::AddCursorAbove => {
            outcome.moved = add_cursor(document, -1)?;
        }

        Action::AddCursorBelow => {
            outcome.moved = add_cursor(document, 1)?;
        }

        Action::Undo => {
            outcome.edited = document.undo()?;
        }

        Action::Redo => {
            outcome.edited = document.redo()?;
        }

        Action::Scroll(delta) => {
            let before = viewport.first_line;
            viewport.scroll_by(*delta, document.buffer().len_lines());
            outcome.scrolled = viewport.first_line != before;
        }

        Action::CenterCursor => {
            let offset = document.selections().primary().head;
            let line = document.buffer().offset_to_line(offset)?;
            let before = viewport.first_line;
            viewport.center_on(line, document.buffer().len_lines());
            outcome.scrolled = viewport.first_line != before;
        }

        Action::Save
        | Action::Close
        | Action::Quit
        | Action::CommandPalette
        | Action::FindFile
        | Action::Search
        | Action::AiPanel => outcome.global = true,
    }

    // Any edit or motion brings the caret back into view: an editor that lets
    // you type off-screen is one you have to scroll manually after every jump.
    if outcome.edited || outcome.moved {
        let offset = document.selections().primary().head;
        let line = document.buffer().offset_to_line(offset)?;
        let column = document.buffer().offset_to_position(offset)?.column;
        let before = (viewport.first_line, viewport.first_column);
        viewport.scroll_to_line(line, 2);
        viewport.scroll_to_column(column, 4);
        outcome.scrolled = (viewport.first_line, viewport.first_column) != before;
    }

    Ok(outcome)
}

/// Delete from each cursor to wherever `target` says, in one transaction.
fn delete_to(
    document: &mut Document,
    target: impl Fn(&nebula_core::TextBuffer, usize) -> usize,
) -> Result<bool> {
    let mut edits = Vec::new();

    for selection in document.selections().iter() {
        // A non-empty selection is what gets deleted, regardless of the motion:
        // that is what every editor does, and it is what the user expects when
        // they have deliberately selected something.
        let range = if selection.is_empty() {
            let to = target(document.buffer(), selection.head);
            nebula_core::position::Range::new(selection.head, to)
        } else {
            selection.range()
        };

        if !range.is_empty() {
            edits.push(Edit::delete(range));
        }
    }

    if edits.is_empty() {
        return Ok(false);
    }

    let transaction = Transaction::from_edits(edits)?;
    document.apply(transaction, false)?;
    Ok(true)
}

/// Remove up to one indent level from every line a cursor is on.
fn outdent(document: &mut Document) -> Result<bool> {
    let mut lines: Vec<usize> = Vec::new();
    for selection in document.selections().iter() {
        let first = document.buffer().offset_to_line(selection.start())?;
        let last = document.buffer().offset_to_line(selection.end())?;
        for line in first..=last {
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
    }
    lines.sort_unstable();

    let mut edits = Vec::new();
    for line in lines {
        let start = document.buffer().line_start(line)?;
        let text = document.buffer().line(line)?;
        // Remove whichever is smaller: one indent level, or the indentation
        // that is actually there. A line indented by two spaces loses two.
        let removable = text.chars().take(INDENT_WIDTH).take_while(|c| *c == ' ').count();
        if removable > 0 {
            edits.push(Edit::delete(nebula_core::position::Range::new(start, start + removable)));
        }
    }

    if edits.is_empty() {
        return Ok(false);
    }

    document.apply(Transaction::from_edits(edits)?, false)?;
    Ok(true)
}

/// Add a cursor one line above or below the existing extremity.
fn add_cursor(document: &mut Document, direction: isize) -> Result<bool> {
    let anchor = if direction < 0 {
        document.selections().iter().map(|s| s.head).min()
    } else {
        document.selections().iter().map(|s| s.head).max()
    };
    let Some(anchor) = anchor else { return Ok(false) };

    let position = document.buffer().offset_to_position(anchor)?;
    let target_line = match direction {
        d if d < 0 => {
            if position.line == 0 {
                return Ok(false);
            }
            position.line - 1
        }
        _ => {
            if position.line + 1 >= document.buffer().len_lines() {
                return Ok(false);
            }
            position.line + 1
        }
    };

    let column = position.column.min(document.buffer().line_len(target_line)?);
    let offset =
        document.buffer().position_to_offset(nebula_core::Position::new(target_line, column))?;
    document.add_cursor(offset);
    Ok(true)
}

/// Move or extend every cursor.
fn move_cursors(
    document: &mut Document,
    motion: Motion,
    viewport: &Viewport,
    extend: bool,
) -> Result<()> {
    let page = viewport.visible_lines.max(1);
    let buffer_lines = document.buffer().len_lines();
    let len = document.buffer().len_chars();

    // Collect first: computing a target needs the buffer, and the closure
    // handed to `transform` cannot borrow it while the set is borrowed mutably.
    let mut targets: Vec<(usize, Option<usize>)> = Vec::new();
    for selection in document.selections().iter() {
        // A plain left/right on a non-empty selection collapses to its edge
        // rather than moving one character from the head — this is the one
        // motion where "collapse" and "move" differ.
        if !extend && !selection.is_empty() && matches!(motion, Motion::Left | Motion::Right) {
            let at =
                if matches!(motion, Motion::Left) { selection.start() } else { selection.end() };
            targets.push((at, None));
            continue;
        }

        let from = selection.head;
        let target = match motion {
            Motion::Left => from.saturating_sub(1),
            Motion::Right => (from + 1).min(len),
            Motion::WordLeft => word::prev_word_boundary(document.buffer(), from),
            Motion::WordRight => word::next_word_boundary(document.buffer(), from),
            Motion::LineStart => smart_line_start(document, from)?,
            Motion::LineEnd => {
                let line = document.buffer().offset_to_line(from)?;
                document.buffer().line_end(line)?
            }
            Motion::DocumentStart => 0,
            Motion::DocumentEnd => len,
            Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown => {
                let delta: isize = match motion {
                    Motion::Up => -1,
                    Motion::Down => 1,
                    Motion::PageUp => -(page as isize),
                    _ => page as isize,
                };
                let position = document.buffer().offset_to_position(from)?;

                // Vertical motion remembers the column it started from, so
                // moving down through a short line and back up returns to where
                // it began instead of sticking to the short line's end.
                let desired = selection.desired_column.unwrap_or(position.column);
                let line = position.line as isize + delta;

                if line < 0 {
                    targets.push((0, Some(desired)));
                    continue;
                }
                let line = (line as usize).min(buffer_lines.saturating_sub(1));
                let column = desired.min(document.buffer().line_len(line)?);
                document.buffer().position_to_offset(nebula_core::Position::new(line, column))?
            }
        };

        let keep_column =
            matches!(motion, Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown).then(
                || {
                    selection.desired_column.unwrap_or_else(|| {
                        document.buffer().offset_to_position(from).map(|p| p.column).unwrap_or(0)
                    })
                },
            );

        targets.push((target, keep_column));
    }

    let mut selections = document.selections().clone();
    let mut index = 0;
    selections.transform(|selection| {
        let (target, desired) = targets[index];
        index += 1;
        let mut moved = if extend { selection.extend_to(target) } else { Selection::caret(target) };
        moved.desired_column = desired;
        moved
    });

    document.set_selections(selections);
    Ok(())
}

/// Home goes to the first non-whitespace character, and to column 0 if it is
/// already there.
///
/// Toggling is what makes one key useful on indented code: the first press
/// lands where the text starts, the second where the line starts.
fn smart_line_start(document: &Document, offset: usize) -> Result<usize> {
    let buffer = document.buffer();
    let line = buffer.offset_to_line(offset)?;
    let start = buffer.line_start(line)?;
    let indent = word::line_indent(buffer, line).chars().count();
    let first_text = start + indent;

    Ok(if offset == first_text { start } else { first_text })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        Document::from_str(text)
    }

    fn viewport() -> Viewport {
        Viewport::new(20, 80)
    }

    /// Drive an action, the way the editor does.
    fn run(action: Action, document: &mut Document) -> Applied {
        apply(&action, document, &mut viewport(), false).unwrap()
    }

    #[test]
    fn typing_a_character_inserts_it() {
        let mut document = doc("");
        let applied = run(Action::Insert("h".to_string()), &mut document);
        assert!(applied.edited);
        assert_eq!(document.text(), "h");
    }

    #[test]
    fn a_plain_letter_is_text_and_a_modified_one_is_a_command() {
        assert_eq!(keymap(&KeyEvent::char('s')), Some(Action::Insert("s".to_string())));
        assert_eq!(keymap(&KeyEvent::with(Key::Char('s'), Modifiers::PRIMARY)), Some(Action::Save));
    }

    #[test]
    fn shifted_letters_are_still_text() {
        // Shift is how half the keyboard is typed; treating it as a modifier
        // that rebinds keys would make capital letters unreachable.
        assert_eq!(
            keymap(&KeyEvent::with(Key::Char('S'), Modifiers::SHIFT)),
            Some(Action::Insert("S".to_string()))
        );
    }

    #[test]
    fn undo_and_redo_are_bound_the_way_the_platform_expects() {
        assert_eq!(keymap(&KeyEvent::with(Key::Char('z'), Modifiers::PRIMARY)), Some(Action::Undo));
        assert_eq!(
            keymap(&KeyEvent::with(Key::Char('z'), Modifiers::PRIMARY_SHIFT)),
            Some(Action::Redo)
        );
    }

    #[test]
    fn an_unbound_key_is_ignored_rather_than_an_error() {
        assert_eq!(keymap(&KeyEvent::new(Key::Function(13))), None);
        assert_eq!(keymap(&KeyEvent::with(Key::Char('j'), Modifiers::PRIMARY)), None);
    }

    #[test]
    fn shift_turns_a_motion_into_a_selection() {
        assert_eq!(keymap(&KeyEvent::new(Key::Left)), Some(Action::Move(Motion::Left)));
        assert_eq!(
            keymap(&KeyEvent::with(Key::Left, Modifiers::SHIFT)),
            Some(Action::Extend(Motion::Left))
        );
    }

    #[test]
    fn the_word_modifier_switches_arrows_to_word_motion() {
        let mut modifiers = Modifiers::WORD;
        assert_eq!(
            keymap(&KeyEvent::with(Key::Right, modifiers)),
            Some(Action::Move(Motion::WordRight))
        );
        modifiers.shift = true;
        assert_eq!(
            keymap(&KeyEvent::with(Key::Right, modifiers)),
            Some(Action::Extend(Motion::WordRight))
        );
    }

    #[test]
    fn backspace_with_the_word_modifier_deletes_a_word() {
        assert_eq!(keymap(&KeyEvent::new(Key::Backspace)), Some(Action::DeleteBackward));
        assert_eq!(
            keymap(&KeyEvent::with(Key::Backspace, Modifiers::WORD)),
            Some(Action::DeleteWordBackward)
        );
    }

    #[test]
    fn newline_keeps_the_current_indentation() {
        let mut document = doc("    let x = 1;");
        document.set_caret(14);
        run(Action::Newline, &mut document);
        assert_eq!(document.text(), "    let x = 1;\n    ");
        // And the caret sits after the inherited indent, ready to type.
        assert_eq!(document.selections().primary().head, 19);
    }

    #[test]
    fn newline_in_unindented_code_adds_no_indentation() {
        let mut document = doc("fn main() {");
        document.set_caret(11);
        run(Action::Newline, &mut document);
        assert_eq!(document.text(), "fn main() {\n");
    }

    #[test]
    fn indent_inserts_a_level_and_outdent_removes_one() {
        let mut document = doc("let x = 1;");
        document.set_caret(0);
        run(Action::Indent, &mut document);
        assert_eq!(document.text(), "    let x = 1;");

        run(Action::Outdent, &mut document);
        assert_eq!(document.text(), "let x = 1;");
    }

    #[test]
    fn outdent_removes_only_what_is_there() {
        // Two spaces of indent lose two, not four, and an unindented line is
        // left alone rather than eating the first characters of its text.
        let mut document = doc("  two\nnone");
        document.set_caret(3);
        run(Action::Outdent, &mut document);
        assert_eq!(document.text(), "two\nnone");

        let applied = run(Action::Outdent, &mut document);
        assert!(!applied.edited, "there was nothing to outdent");
        assert_eq!(document.text(), "two\nnone");
    }

    #[test]
    fn outdent_over_a_multi_line_selection_touches_every_line() {
        let mut document = doc("    a\n    b\n    c");
        document.set_selections(SelectionSet::single(Selection::new(0, 17)));
        run(Action::Outdent, &mut document);
        assert_eq!(document.text(), "a\nb\nc");
    }

    #[test]
    fn deleting_a_word_backward_stops_at_the_boundary() {
        let mut document = doc("hello world");
        document.set_caret(11);
        run(Action::DeleteWordBackward, &mut document);
        assert_eq!(document.text(), "hello ");
    }

    #[test]
    fn deleting_a_word_forward_stops_at_the_boundary() {
        let mut document = doc("hello world");
        document.set_caret(0);
        run(Action::DeleteWordForward, &mut document);
        // The boundary is the start of the *next* word, not the end of this
        // one: forward word-delete takes the separating space with it, which is
        // what makes repeated presses delete word-by-word cleanly.
        assert_eq!(document.text(), "world");
    }

    #[test]
    fn a_word_delete_over_a_selection_deletes_the_selection() {
        // The user selected something deliberately; a motion-based delete must
        // not reach past it.
        let mut document = doc("alpha beta gamma");
        document.set_selections(SelectionSet::single(Selection::new(6, 10)));
        run(Action::DeleteWordBackward, &mut document);
        assert_eq!(document.text(), "alpha  gamma");
    }

    #[test]
    fn delete_to_line_end_joins_lines_when_already_at_the_end() {
        let mut document = doc("first\nsecond");
        document.set_caret(2);
        run(Action::DeleteToLineEnd, &mut document);
        assert_eq!(document.text(), "fi\nsecond");

        run(Action::DeleteToLineEnd, &mut document);
        assert_eq!(document.text(), "fisecond");
    }

    #[test]
    fn home_toggles_between_the_text_and_the_line_start() {
        let mut document = doc("    indented");
        document.set_caret(9);

        run(Action::Move(Motion::LineStart), &mut document);
        assert_eq!(document.selections().primary().head, 4, "first press: start of text");

        run(Action::Move(Motion::LineStart), &mut document);
        assert_eq!(document.selections().primary().head, 0, "second press: column 0");

        run(Action::Move(Motion::LineStart), &mut document);
        assert_eq!(document.selections().primary().head, 4, "and back again");
    }

    #[test]
    fn vertical_motion_remembers_the_column_across_a_short_line() {
        // Down through a short line and back up must return to column 10, not
        // stick to the short line's end. This is the single most-noticed
        // detail of cursor movement.
        let mut document = doc("0123456789abcdef\nshort\n0123456789abcdef");
        document.set_caret(12);

        let mut view = viewport();
        apply(&Action::Move(Motion::Down), &mut document, &mut view, false).unwrap();
        assert_eq!(
            document
                .buffer()
                .offset_to_position(document.selections().primary().head)
                .unwrap()
                .column,
            5
        );

        apply(&Action::Move(Motion::Down), &mut document, &mut view, false).unwrap();
        assert_eq!(
            document
                .buffer()
                .offset_to_position(document.selections().primary().head)
                .unwrap()
                .column,
            12
        );
    }

    #[test]
    fn horizontal_motion_forgets_the_remembered_column() {
        let mut document = doc("0123456789\nshort\n0123456789");
        document.set_caret(8);
        let mut view = viewport();

        apply(&Action::Move(Motion::Down), &mut document, &mut view, false).unwrap();
        apply(&Action::Move(Motion::Left), &mut document, &mut view, false).unwrap();
        assert_eq!(
            document.selections().primary().desired_column,
            None,
            "a horizontal move must clear the sticky column"
        );

        // So the third line is entered at column 4 — where the caret actually
        // is — rather than being pulled back to the remembered column 8.
        apply(&Action::Move(Motion::Down), &mut document, &mut view, false).unwrap();
        let column = document
            .buffer()
            .offset_to_position(document.selections().primary().head)
            .unwrap()
            .column;
        assert_eq!(column, 4);
    }

    #[test]
    fn left_on_a_selection_collapses_to_its_start() {
        let mut document = doc("hello world");
        document.set_selections(SelectionSet::single(Selection::new(2, 8)));
        run(Action::Move(Motion::Left), &mut document);
        assert_eq!(document.selections().primary().head, 2);
        assert!(document.selections().primary().is_empty());
    }

    #[test]
    fn right_on_a_selection_collapses_to_its_end() {
        let mut document = doc("hello world");
        document.set_selections(SelectionSet::single(Selection::new(2, 8)));
        run(Action::Move(Motion::Right), &mut document);
        assert_eq!(document.selections().primary().head, 8);
    }

    #[test]
    fn extending_keeps_the_anchor() {
        let mut document = doc("hello");
        document.set_caret(0);
        run(Action::Extend(Motion::Right), &mut document);
        run(Action::Extend(Motion::Right), &mut document);

        let selection = document.selections().primary();
        assert_eq!(selection.anchor, 0);
        assert_eq!(selection.head, 2);
    }

    #[test]
    fn motion_stops_at_the_documents_edges() {
        let mut document = doc("ab");
        document.set_caret(0);
        run(Action::Move(Motion::Left), &mut document);
        assert_eq!(document.selections().primary().head, 0);

        document.set_caret(2);
        run(Action::Move(Motion::Right), &mut document);
        assert_eq!(document.selections().primary().head, 2);
    }

    #[test]
    fn select_all_covers_the_document() {
        let mut document = doc("one\ntwo\nthree");
        run(Action::SelectAll, &mut document);
        let selection = document.selections().primary();
        assert_eq!(selection.start(), 0);
        assert_eq!(selection.end(), 13);
    }

    #[test]
    fn select_line_includes_the_newline_so_deleting_removes_the_line() {
        let mut document = doc("first\nsecond\nthird");
        document.set_caret(8);
        run(Action::SelectLine, &mut document);
        run(Action::DeleteBackward, &mut document);
        assert_eq!(document.text(), "first\nthird");
    }

    #[test]
    fn select_word_takes_the_word_under_the_caret() {
        let mut document = doc("alpha beta gamma");
        document.set_caret(8);
        run(Action::SelectWord, &mut document);
        let selection = document.selections().primary();
        assert_eq!(&document.text()[selection.start()..selection.end()], "beta");
    }

    #[test]
    fn escape_collapses_to_one_caret() {
        let mut document = doc("one\ntwo\nthree");
        document.set_caret(0);
        run(Action::AddCursorBelow, &mut document);
        assert_eq!(document.selections().len(), 2);

        run(Action::CollapseSelection, &mut document);
        assert_eq!(document.selections().len(), 1);
        assert!(document.selections().primary().is_empty());
    }

    #[test]
    fn cursors_can_be_added_above_and_below() {
        let mut document = doc("aaa\nbbb\nccc");
        document.set_caret(5);

        assert!(run(Action::AddCursorBelow, &mut document).moved);
        assert_eq!(document.selections().len(), 2);
        assert!(run(Action::AddCursorAbove, &mut document).moved);
        assert_eq!(document.selections().len(), 3);

        // One cursor per line, all at column 1.
        for selection in document.selections().iter() {
            assert_eq!(document.buffer().offset_to_position(selection.head).unwrap().column, 1);
        }
    }

    #[test]
    fn adding_a_cursor_past_the_last_line_does_nothing() {
        let mut document = doc("only");
        document.set_caret(0);
        assert!(!run(Action::AddCursorBelow, &mut document).moved);
        assert!(!run(Action::AddCursorAbove, &mut document).moved);
        assert_eq!(document.selections().len(), 1);
    }

    #[test]
    fn a_cursor_added_below_lands_on_the_short_lines_end() {
        let mut document = doc("0123456789\nab");
        document.set_caret(8);
        run(Action::AddCursorBelow, &mut document);

        let heads: Vec<usize> = document.selections().iter().map(|s| s.head).collect();
        assert!(heads.contains(&13), "the second cursor should sit at the end of `ab`: {heads:?}");
    }

    #[test]
    fn typing_at_every_cursor_edits_every_line() {
        let mut document = doc("aaa\nbbb\nccc");
        document.set_caret(0);
        run(Action::AddCursorBelow, &mut document);
        run(Action::AddCursorBelow, &mut document);
        assert_eq!(document.selections().len(), 3);

        run(Action::Insert(">".to_string()), &mut document);
        assert_eq!(document.text(), ">aaa\n>bbb\n>ccc");
    }

    #[test]
    fn undo_reverses_an_edit_and_redo_reapplies_it() {
        let mut document = doc("start");
        document.set_caret(5);
        run(Action::Insert("!".to_string()), &mut document);
        assert_eq!(document.text(), "start!");

        assert!(run(Action::Undo, &mut document).edited);
        assert_eq!(document.text(), "start");

        assert!(run(Action::Redo, &mut document).edited);
        assert_eq!(document.text(), "start!");
    }

    #[test]
    fn undo_with_nothing_to_undo_reports_no_change() {
        let mut document = doc("untouched");
        assert!(!run(Action::Undo, &mut document).edited);
    }

    #[test]
    fn editing_scrolls_the_caret_back_into_view() {
        let text = (0..500).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let mut document = doc(&text);
        let mut view = Viewport::new(20, 80);
        view.first_line = 400;

        document.set_caret(0);
        let applied =
            apply(&Action::Insert("x".to_string()), &mut document, &mut view, false).unwrap();

        assert!(applied.scrolled);
        assert!(view.contains_line(0), "the caret's line must be visible after typing");
    }

    #[test]
    fn scrolling_does_not_move_the_caret() {
        let text = (0..100).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let mut document = doc(&text);
        document.set_caret(0);
        let mut view = Viewport::new(10, 80);

        let applied = apply(&Action::Scroll(5), &mut document, &mut view, false).unwrap();
        assert!(applied.scrolled);
        assert!(!applied.moved);
        assert_eq!(document.selections().primary().head, 0);
        assert_eq!(view.first_line, 5);
    }

    #[test]
    fn page_motion_moves_by_the_viewports_height() {
        let text = (0..200).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let mut document = doc(&text);
        document.set_caret(0);
        let mut view = Viewport::new(30, 80);

        apply(&Action::Move(Motion::PageDown), &mut document, &mut view, false).unwrap();
        let line = document.buffer().offset_to_line(document.selections().primary().head).unwrap();
        assert_eq!(line, 30);
    }

    #[test]
    fn global_actions_do_not_touch_the_document() {
        let mut document = doc("unchanged");
        for action in [Action::Save, Action::Quit, Action::CommandPalette, Action::Search] {
            let applied = run(action, &mut document);
            assert!(applied.global);
            assert!(!applied.edited);
            assert!(!applied.moved);
        }
        assert_eq!(document.text(), "unchanged");
        assert!(!document.is_modified());
    }

    #[test]
    fn edit_actions_are_classified_as_edits() {
        assert!(Action::Insert("x".into()).is_edit());
        assert!(Action::Undo.is_edit());
        assert!(!Action::Move(Motion::Left).is_edit());
        assert!(!Action::Save.is_edit());
        assert!(Action::Save.is_global());
        assert!(!Action::Newline.is_global());
    }

    #[test]
    fn a_frame_is_only_redrawn_when_something_changed() {
        assert!(!Applied::default().needs_redraw());
        assert!(Applied { edited: true, ..Applied::default() }.needs_redraw());
        assert!(Applied { scrolled: true, ..Applied::default() }.needs_redraw());
        assert!(!Applied { global: true, ..Applied::default() }.needs_redraw());
    }

    #[test]
    fn a_whole_typed_line_arrives_intact() {
        // The end-to-end shape: key events in, text out, nothing in between.
        let mut document = doc("");
        let mut view = viewport();

        for key in "fn main() {".chars() {
            let event = KeyEvent::char(key);
            let action = keymap(&event).expect("every printable character is bound");
            apply(&action, &mut document, &mut view, true).unwrap();
        }
        apply(&keymap(&KeyEvent::new(Key::Enter)).unwrap(), &mut document, &mut view, false)
            .unwrap();
        for key in "    body".chars() {
            let action = keymap(&KeyEvent::char(key)).unwrap();
            apply(&action, &mut document, &mut view, true).unwrap();
        }

        assert_eq!(document.text(), "fn main() {\n    body");
    }

    #[test]
    fn unicode_text_is_measured_in_characters_not_bytes() {
        let mut document = doc("héllo wörld");
        document.set_caret(11);
        run(Action::DeleteWordBackward, &mut document);
        assert_eq!(document.text(), "héllo ");
    }

    #[test]
    fn the_primary_modifier_matches_the_platform() {
        let mut modifiers = Modifiers::NONE;
        if cfg!(target_os = "macos") {
            modifiers.meta = true;
        } else {
            modifiers.ctrl = true;
        }
        assert!(modifiers.primary());
        assert!(!Modifiers::NONE.primary());
        assert!(Modifiers::SHIFT.is_plain());
    }
}
