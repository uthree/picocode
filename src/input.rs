//! Terminal input plumbing: the blocking read thread, paste detection and
//! collapsing, and cursor math for the multi-line input box.

use std::time::Duration;

use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;

/// Pastes at least this many lines (or chars) long are collapsed into a
/// `[Pasted text #n +N lines]` placeholder in the input box.
const PASTE_COLLAPSE_LINES: usize = 6;
const PASTE_COLLAPSE_CHARS: usize = 500;

pub fn spawn_input_thread() -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel(64);
    std::thread::spawn(move || {
        use ratatui::crossterm::event;
        // Key releases (reported on Windows) are ignored by the app and
        // would break the paste-run detection below, so drop them here.
        let release = |ev: &Event| matches!(ev, Event::Key(k) if k.kind == KeyEventKind::Release);
        loop {
            let Ok(first) = event::read() else {
                return;
            };
            if release(&first) {
                continue;
            }
            // Drain everything already queued: a clipboard paste delivers its
            // characters in one burst, while human keystrokes arrive one per
            // read. The batch lets `coalesce_paste` tell the two apart on
            // terminals without bracketed paste.
            let mut batch = vec![first];
            while batch.len() < 4096 && event::poll(Duration::ZERO).unwrap_or(false) {
                match event::read() {
                    Ok(ev) if release(&ev) => {}
                    Ok(ev) => batch.push(ev),
                    Err(_) => return,
                }
            }
            for ev in coalesce_paste(batch) {
                if tx.blocking_send(ev).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

/// The text a key event would type, if any: plain character, Enter and Tab
/// presses, and bracketed-paste events.
fn textual(ev: &Event) -> Option<String> {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => {
            let plain = k.modifiers.difference(KeyModifiers::SHIFT).is_empty();
            match k.code {
                KeyCode::Char(c) if plain => Some(c.to_string()),
                KeyCode::Enter if plain => Some("\n".to_string()),
                KeyCode::Tab if plain => Some("\t".to_string()),
                _ => None,
            }
        }
        Event::Paste(s) => Some(s.clone()),
        _ => None,
    }
}

/// Paste detection for terminals without bracketed paste: within a burst of
/// simultaneously-arriving events, a run of plain text keys with a newline
/// *inside* it can only be a multi-line paste. Such runs are replaced by a
/// single `Event::Paste` so the newlines are inserted instead of each Enter
/// submitting a message. Runs whose only newlines trail at the end (text
/// then Enter — e.g. keystrokes bunched up by a laggy connection, or a
/// scripted command) replay as normal key presses so the Enter still
/// submits. Anything else passes through unchanged.
fn coalesce_paste(batch: Vec<Event>) -> Vec<Event> {
    if batch.len() < 2 {
        return batch;
    }

    fn flush(out: &mut Vec<Event>, run: &mut Vec<Event>, text: &mut String) {
        if text.trim_end_matches('\n').contains('\n') {
            run.clear();
            out.push(Event::Paste(std::mem::take(text)));
        } else {
            out.append(run);
            text.clear();
        }
    }

    let mut out: Vec<Event> = Vec::new();
    let mut run: Vec<Event> = Vec::new();
    let mut text = String::new();
    for ev in batch {
        match textual(&ev) {
            Some(t) => {
                text.push_str(&t);
                run.push(ev);
            }
            None => {
                flush(&mut out, &mut run, &mut text);
                out.push(ev);
            }
        }
    }
    flush(&mut out, &mut run, &mut text);
    out
}

/// Placeholder for the n-th pasted block, or None when the paste is short
/// enough to go into the input verbatim.
pub fn paste_placeholder(text: &str, n: usize) -> Option<String> {
    let lines = text.lines().count().max(1);
    if lines < PASTE_COLLAPSE_LINES && text.chars().count() < PASTE_COLLAPSE_CHARS {
        return None;
    }
    let plural = if lines == 1 { "line" } else { "lines" };
    Some(format!("[Pasted text #{n} +{lines} {plural}]"))
}

/// Replace every paste placeholder in a submitted message with its full text.
/// Placeholders the user edited no longer match and are sent as-is.
pub fn expand_pastes(text: &str, pasted: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (placeholder, content) in pasted {
        if out.contains(placeholder.as_str()) {
            out = out.replace(placeholder.as_str(), content);
        }
    }
    out
}

/// (row, column) of a char cursor within a multi-line string, in chars.
pub fn line_col(text: &str, cursor: usize) -> (usize, usize) {
    let mut row = 0;
    let mut col = 0;
    for c in text.chars().take(cursor) {
        if c == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

/// Char cursor for a (row, column) position, clamping the column to the
/// line's length (used by Up/Down in the input box).
pub fn cursor_at(text: &str, row: usize, col: usize) -> usize {
    let mut cursor = 0;
    for (i, line) in text.split('\n').enumerate() {
        let len = line.chars().count();
        if i == row {
            return cursor + col.min(len);
        }
        cursor += len + 1;
    }
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    #[test]
    fn line_col_tracks_newlines() {
        assert_eq!(line_col("abc", 2), (0, 2));
        assert_eq!(line_col("ab\ncd", 3), (1, 0));
        assert_eq!(line_col("ab\ncd", 5), (1, 2));
        assert_eq!(line_col("", 0), (0, 0));
        // Multi-byte chars count as one column.
        assert_eq!(line_col("あい\nう", 4), (1, 1));
    }

    #[test]
    fn cursor_at_clamps_to_line_length() {
        let text = "long line\nab\nmiddle";
        assert_eq!(cursor_at(text, 0, 4), 4);
        // Column clamped to the shorter line.
        assert_eq!(cursor_at(text, 1, 7), 12);
        assert_eq!(cursor_at(text, 2, 0), 13);
        // A row past the end lands at the end of the text.
        assert_eq!(cursor_at(text, 9, 0), text.chars().count());
    }

    #[test]
    fn line_col_and_cursor_at_roundtrip() {
        let text = "one\ntwo three\nよん";
        for cursor in 0..=text.chars().count() {
            let (row, col) = line_col(text, cursor);
            assert_eq!(cursor_at(text, row, col), cursor);
        }
    }

    #[test]
    fn long_pastes_collapse_into_placeholders() {
        // Short pastes stay verbatim.
        assert_eq!(paste_placeholder("one\ntwo", 1), None);
        assert_eq!(paste_placeholder("short", 3), None);
        // Collapse by line count…
        let six_lines = "a\nb\nc\nd\ne\nf";
        assert_eq!(
            paste_placeholder(six_lines, 1).as_deref(),
            Some("[Pasted text #1 +6 lines]")
        );
        // …or by size, even on a single line.
        let big = "x".repeat(600);
        assert_eq!(
            paste_placeholder(&big, 2).as_deref(),
            Some("[Pasted text #2 +1 line]")
        );

        let pasted = vec![
            (
                "[Pasted text #1 +6 lines]".to_string(),
                six_lines.to_string(),
            ),
            ("[Pasted text #2 +1 line]".to_string(), big.clone()),
        ];
        // Expansion swaps every placeholder for its full text.
        assert_eq!(
            expand_pastes("see [Pasted text #1 +6 lines] end", &pasted),
            format!("see {six_lines} end")
        );
        assert_eq!(
            expand_pastes(
                "[Pasted text #1 +6 lines]\n[Pasted text #2 +1 line]",
                &pasted
            ),
            format!("{six_lines}\n{big}")
        );
        // Edited placeholders no longer match and are left alone.
        assert_eq!(
            expand_pastes("[Pasted text #1 +6 line]", &pasted),
            "[Pasted text #1 +6 line]"
        );
    }

    #[test]
    fn paste_bursts_coalesce_into_paste_events() {
        let key = |c: char| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let enter = || Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let up = || Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        // A burst with a newline inside the text can only be a paste.
        let out = coalesce_paste(vec![key('a'), key('b'), enter(), key('c')]);
        assert!(matches!(&out[..], [Event::Paste(s)] if s.as_str() == "ab\nc"));

        // A lone Enter keeps submitting.
        let out = coalesce_paste(vec![enter()]);
        assert!(matches!(&out[..], [Event::Key(_)]));

        // A fast burst without newlines passes through unchanged.
        let out = coalesce_paste(vec![key('h'), key('i')]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|e| matches!(e, Event::Key(_))));

        // Text with only a trailing Enter is a typed command bunched up in
        // transit (or a scripted one) — it replays and still submits.
        let out = coalesce_paste(vec![key('l'), key('s'), enter()]);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|e| matches!(e, Event::Key(_))));

        // Non-text events break the run.
        let out = coalesce_paste(vec![key('a'), enter(), key('b'), up(), key('c')]);
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], Event::Paste(s) if s.as_str() == "a\nb"));
        assert!(matches!(&out[1], Event::Key(k) if k.code == KeyCode::Up));
        assert!(matches!(&out[2], Event::Key(k) if k.code == KeyCode::Char('c')));

        // A bracketed-paste event merges with keys from the same burst.
        let out = coalesce_paste(vec![Event::Paste("x\ny".into()), key('z')]);
        assert!(matches!(&out[..], [Event::Paste(s)] if s.as_str() == "x\nyz"));

        // Modified keys (e.g. Ctrl+C in a burst) are never swallowed.
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let out = coalesce_paste(vec![key('a'), enter(), ctrl_c]);
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[2], Event::Key(k) if k.modifiers == KeyModifiers::CONTROL));
    }
}
