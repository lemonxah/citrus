//! Minimal vim-style modal editing for the code editor.
//!
//! Core motions and operators only — enough for day-to-day editing; not a full
//! vim. The code editor stores [`VimState`] in egui memory per file and, when
//! enabled and focused, feeds captured key/text events through [`handle`]
//! before the `TextEdit` sees them (Normal/Visual modes consume input; Insert
//! mode lets the `TextEdit` type normally and only watches for Escape).
//!
//! Supported: modes Normal / Insert / Visual / Visual-line; motions
//! h j k l w b e 0 ^ $ gg G (with counts); inserts i a I A o O; edits
//! x D C dd cc yy dw cw yw d$/c$/y$ p P; visual d y c x. Cursor is modelled as
//! a char index (block-cursor semantics are approximated).

use egui::{Event, Key};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum VimMode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
    /// `:` command-line entry (ex commands).
    Command,
}

impl VimMode {
    pub fn label(self) -> &'static str {
        match self {
            VimMode::Normal => "NORMAL",
            VimMode::Insert => "INSERT",
            VimMode::Visual => "VISUAL",
            VimMode::VisualLine => "V-LINE",
            VimMode::Command => "COMMAND",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Delete,
    Change,
    Yank,
}

#[derive(Clone, Default)]
pub struct VimState {
    pub mode: VimMode,
    count: Option<usize>,
    pending: Option<Op>,
    awaiting_g: bool,
    register: String,
    register_linewise: bool,
    visual_anchor: usize,
    /// Command-line buffer (text after `:`), valid in [`VimMode::Command`].
    pub cmdline: String,
    /// Original text stashed while a `:` command is being typed, so live
    /// substitution preview can be reverted on cancel and applied cleanly on
    /// commit. Some only during [`VimMode::Command`].
    pub preview_base: Option<String>,
}

#[derive(Default)]
pub struct VimOutcome {
    /// New primary cursor (char index), if it moved.
    pub cursor: Option<usize>,
    /// Visual selection (anchor, cursor) char indices.
    pub selection: Option<(usize, usize)>,
    pub text_changed: bool,
    /// `:w` requested a save.
    pub save: bool,
    /// `:q` requested the tab be closed.
    pub close: bool,
    /// `u` — undo the last edit (the editor owns the snapshot stack).
    pub undo: bool,
    /// Ctrl+R — redo.
    pub redo: bool,
    /// `gd` — go to definition at the cursor.
    pub goto_def: bool,
    /// `gr` — list references at the cursor.
    pub goto_refs: bool,
}

/// Process captured events for one frame. `cursor` is the current primary
/// cursor char index. Mutates `text` for edits; returns the new cursor /
/// selection to apply to the TextEdit.
pub fn handle(state: &mut VimState, text: &mut String, cursor: usize, events: &[Event]) -> VimOutcome {
    let mut cs: Vec<char> = text.chars().collect();
    let mut cur = cursor.min(cs.len());
    let mut changed = false;
    let mut out = VimOutcome::default();

    for ev in events {
        if state.mode == VimMode::Command {
            command_event(state, &mut cs, &mut cur, &mut changed, &mut out, ev);
            continue;
        }
        match ev {
            Event::Text(t) => {
                for ch in t.chars() {
                    step(state, &mut cs, &mut cur, &mut changed, &mut out, ch);
                }
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => match key {
                // Ctrl+R: redo (the editor owns the snapshot stack).
                Key::R if modifiers.ctrl || modifiers.command => out.redo = true,
                Key::Escape => to_normal(state, &mut cur, &cs),
                Key::Backspace | Key::ArrowLeft => cur = motion(&cs, cur, 'h', 1, state),
                Key::ArrowRight => cur = motion(&cs, cur, 'l', 1, state),
                Key::ArrowUp => cur = motion(&cs, cur, 'k', 1, state),
                Key::ArrowDown | Key::Enter => cur = motion(&cs, cur, 'j', 1, state),
                _ => {}
            },
            _ => {}
        }
    }

    if changed {
        *text = cs.iter().collect();
        out.text_changed = true;
    }
    if state.mode == VimMode::Normal {
        cur = clamp_on_char(&cs, cur);
    }
    out.cursor = Some(cur.min(cs.len()));
    if matches!(state.mode, VimMode::Visual | VimMode::VisualLine) {
        out.selection = Some(visual_range(state, cur, &cs));
    }
    out
}

fn to_normal(state: &mut VimState, cur: &mut usize, cs: &[char]) {
    if state.mode == VimMode::Insert {
        // vim steps left when leaving insert.
        *cur = motion(cs, *cur, 'h', 1, state);
    }
    state.mode = VimMode::Normal;
    state.pending = None;
    state.count = None;
    state.awaiting_g = false;
}

/// Handle one event while in command-line (`:`) mode: edit the buffer, or run
/// it on Enter / cancel on Escape.
fn command_event(
    state: &mut VimState,
    cs: &mut Vec<char>,
    cur: &mut usize,
    changed: &mut bool,
    out: &mut VimOutcome,
    ev: &Event,
) {
    match ev {
        Event::Text(t) => {
            for ch in t.chars() {
                if !ch.is_control() {
                    state.cmdline.push(ch);
                }
            }
        }
        Event::Key {
            key: Key::Backspace,
            pressed: true,
            ..
        } => {
            state.cmdline.pop();
        }
        Event::Key {
            key: Key::Escape,
            pressed: true,
            ..
        } => {
            state.cmdline.clear();
            state.mode = VimMode::Normal;
        }
        Event::Key {
            key: Key::Enter,
            pressed: true,
            ..
        } => {
            let cmd = std::mem::take(&mut state.cmdline);
            execute_command(cs, cur, changed, out, cmd.trim());
            state.mode = VimMode::Normal;
        }
        _ => {}
    }
}

/// Run an ex command (the text after `:`). Supports `w` / `q` / `wq` / `x`,
/// a bare line number, and `[%]s/pat/rep/[g]` substitution (literal patterns).
fn execute_command(
    cs: &mut Vec<char>,
    cur: &mut usize,
    changed: &mut bool,
    out: &mut VimOutcome,
    cmd: &str,
) {
    if cmd.is_empty() {
        return;
    }
    // Bare line number: jump there.
    if let Ok(n) = cmd.parse::<usize>() {
        *cur = nth_line_start(cs, n.saturating_sub(1));
        return;
    }
    match cmd {
        "w" | "write" => out.save = true,
        "q" | "q!" | "quit" => out.close = true,
        "wq" | "x" | "wq!" => {
            out.save = true;
            out.close = true;
        }
        _ => {
            let (whole, rest) = if let Some(r) = cmd.strip_prefix("%s") {
                (true, Some(r))
            } else if let Some(r) = cmd.strip_prefix('s') {
                (false, Some(r))
            } else {
                (false, None)
            };
            if let Some(rest) = rest {
                run_substitute(cs, cur, changed, whole, rest);
            }
        }
    }
}

/// Parsed `[%]s/pat/rep/[flags]` command.
struct Subst<'a> {
    whole: bool,
    pat: &'a str,
    rep: &'a str,
    has_rep: bool,
    global: bool,
}

/// Parse `[%]s<delim>pat<delim>rep<delim>flags`. The delimiter is the char
/// after `s`/`%s` and must be punctuation. Returns None if `rest` isn't a
/// substitution.
fn parse_subst(whole: bool, rest: &str) -> Option<Subst<'_>> {
    let delim = rest.chars().next()?;
    if delim.is_alphanumeric() || delim.is_whitespace() {
        return None;
    }
    let parts: Vec<&str> = rest[delim.len_utf8()..].split(delim).collect();
    let pat = parts.first().filter(|p| !p.is_empty())?;
    Some(Subst {
        whole,
        pat,
        rep: parts.get(1).copied().unwrap_or(""),
        has_rep: parts.len() >= 2,
        global: parts.get(2).is_some_and(|f| f.contains('g')),
    })
}

/// Result of previewing a substitution against a base text.
pub struct SubstPreview {
    /// The text with the substitution applied (or the base when only a pattern
    /// has been typed).
    pub text: String,
    /// Char ranges to highlight in `text` (replaced spans, or match spans when
    /// no replacement is given yet).
    pub highlights: Vec<(usize, usize)>,
    /// True once a replacement is being applied (vs just highlighting matches).
    pub replaced: bool,
}

/// Apply a regex substitution line by line, tracking the char ranges of the
/// replacements in the output (for highlighting). Mirrors what commit does.
fn apply_subst(
    base: &str,
    re: &regex::Regex,
    rep: &str,
    whole: bool,
    global: bool,
    cur_line: usize,
) -> (String, Vec<(usize, usize)>) {
    let mut out = String::new();
    let mut ranges = Vec::new();
    let mut off = 0usize; // chars written so far
    for (idx, line) in base.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
            off += 1;
        }
        if !(whole || idx == cur_line) {
            out.push_str(line);
            off += line.chars().count();
            continue;
        }
        let mut last = 0usize; // byte cursor in `line`
        let mut n = 0;
        for caps in re.captures_iter(line) {
            if !global && n >= 1 {
                break;
            }
            let m = caps.get(0).unwrap();
            let prefix = &line[last..m.start()];
            out.push_str(prefix);
            off += prefix.chars().count();
            let mut rep_str = String::new();
            caps.expand(rep, &mut rep_str);
            let start = off;
            out.push_str(&rep_str);
            off += rep_str.chars().count();
            ranges.push((start, off));
            last = m.end();
            n += 1;
        }
        let tail = &line[last..];
        out.push_str(tail);
        off += tail.chars().count();
    }
    (out, ranges)
}

/// Char ranges of every match (for pattern-only highlighting).
fn match_ranges(base: &str, re: &regex::Regex, whole: bool, cur_line: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut off = 0usize;
    for (idx, line) in base.split('\n').enumerate() {
        if idx > 0 {
            off += 1;
        }
        let line_start = off;
        if whole || idx == cur_line {
            for m in re.find_iter(line) {
                let s = line_start + line[..m.start()].chars().count();
                let e = line_start + line[..m.end()].chars().count();
                ranges.push((s, e));
            }
        }
        off += line.chars().count();
    }
    ranges
}

fn cur_line_of(base: &str, cur_char: usize) -> usize {
    base.chars().take(cur_char).filter(|&c| c == '\n').count()
}

/// Compute a live preview of a substitution command against `base`. Returns
/// None if `cmd` isn't a substitution. On a bad regex, returns the base text
/// unchanged with no highlights.
pub fn preview_substitute(base: &str, cmd: &str, cur_char: usize) -> Option<SubstPreview> {
    let (whole, rest) = if let Some(r) = cmd.strip_prefix("%s") {
        (true, r)
    } else if let Some(r) = cmd.strip_prefix('s') {
        (false, r)
    } else {
        return None;
    };
    let sub = parse_subst(whole, rest)?;
    let re = match regex::Regex::new(sub.pat) {
        Ok(re) => re,
        Err(_) => {
            return Some(SubstPreview {
                text: base.to_string(),
                highlights: Vec::new(),
                replaced: false,
            });
        }
    };
    let cur_line = cur_line_of(base, cur_char);
    if sub.has_rep {
        let (text, highlights) = apply_subst(base, &re, sub.rep, sub.whole, sub.global, cur_line);
        Some(SubstPreview {
            text,
            highlights,
            replaced: true,
        })
    } else {
        Some(SubstPreview {
            highlights: match_ranges(base, &re, sub.whole, cur_line),
            text: base.to_string(),
            replaced: false,
        })
    }
}

/// Parse and apply `/pat/rep/[g]` (the part after `s`/`%s`), committing into
/// `cs`. `pat` is a regex; `rep` may use `$1`/`${name}` capture refs.
fn run_substitute(cs: &mut Vec<char>, cur: &mut usize, changed: &mut bool, whole: bool, rest: &str) {
    let Some(sub) = parse_subst(whole, rest) else {
        return;
    };
    let re = match regex::Regex::new(sub.pat) {
        Ok(re) => re,
        Err(e) => {
            tracing::warn!("substitute: bad regex {:?}: {e}", sub.pat);
            return;
        }
    };
    let base: String = cs.iter().collect();
    let cur_line = cur_line_of(&base, *cur);
    let (text, ranges) = apply_subst(&base, &re, sub.rep, sub.whole, sub.global, cur_line);
    if !ranges.is_empty() {
        *cs = text.chars().collect();
        *cur = (*cur).min(cs.len());
        *changed = true;
    }
}

/// One command character.
fn step(
    state: &mut VimState,
    cs: &mut Vec<char>,
    cur: &mut usize,
    changed: &mut bool,
    out: &mut VimOutcome,
    ch: char,
) {
    // Count prefix (a leading 0 is the motion, not a count digit).
    if ch.is_ascii_digit() && !(ch == '0' && state.count.is_none()) {
        let d = ch as usize - '0' as usize;
        state.count = Some(state.count.unwrap_or(0).saturating_mul(10) + d);
        return;
    }

    // `g` prefix: wait for the second key, preserving any pending count so
    // `{n}gg` jumps to line n (the count must survive the first `g`).
    if state.awaiting_g {
        state.awaiting_g = false;
        let count = state.count.take();
        match ch {
            'g' => {
                // gg: first line (or line `count` if given).
                let line = count.map(|n| n.saturating_sub(1)).unwrap_or(0);
                *cur = nth_line_start(cs, line);
            }
            'd' => out.goto_def = true,
            'r' => out.goto_refs = true,
            _ => {}
        }
        return;
    }
    if ch == 'g' {
        state.awaiting_g = true;
        return;
    }

    let explicit = state.count;
    let count = state.count.take().unwrap_or(1);

    // Visual mode: motions extend; operators act on the selection.
    if matches!(state.mode, VimMode::Visual | VimMode::VisualLine) {
        match ch {
            'h' | 'l' | 'j' | 'k' | '0' | '^' | '$' | 'w' | 'b' | 'e' => {
                *cur = motion(cs, *cur, ch, count, state);
            }
            'G' => *cur = goto_last_or(cs, explicit),
            'v' => state.mode = VimMode::Normal,
            'V' => state.mode = VimMode::VisualLine,
            'd' | 'x' | 'y' | 'c' => {
                let (a, b) = visual_range(state, *cur, cs);
                let linewise = state.mode == VimMode::VisualLine;
                yank(state, cs, a, b, linewise);
                if ch != 'y' {
                    delete_range(cs, a, b, changed);
                    *cur = a;
                }
                state.mode = if ch == 'c' { VimMode::Insert } else { VimMode::Normal };
                if ch == 'y' {
                    *cur = a;
                }
            }
            'p' => {
                // Paste over the selection: delete it, then insert the register
                // where it started.
                let (a, b) = visual_range(state, *cur, cs);
                delete_range(cs, a, b, changed);
                *cur = a.min(cs.len());
                paste(state, cs, cur, changed, false);
                state.mode = VimMode::Normal;
            }
            _ => {}
        }
        return;
    }

    // Pending operator awaiting a motion (d/c/y).
    if let Some(op) = state.pending.take() {
        operator_motion(state, cs, cur, changed, op, ch, count);
        return;
    }

    match ch {
        'h' | 'l' | 'j' | 'k' | '0' | '^' | '$' | 'w' | 'b' | 'e' => {
            *cur = motion(cs, *cur, ch, count, state);
        }
        'G' => *cur = goto_last_or(cs, explicit),
        ':' => {
            state.mode = VimMode::Command;
            state.cmdline.clear();
        }
        'i' => state.mode = VimMode::Insert,
        'I' => {
            *cur = first_non_blank(cs, *cur);
            state.mode = VimMode::Insert;
        }
        'a' => {
            if *cur < line_end(cs, *cur) {
                *cur += 1;
            }
            state.mode = VimMode::Insert;
        }
        'A' => {
            *cur = line_end(cs, *cur);
            state.mode = VimMode::Insert;
        }
        'o' => {
            let e = line_end(cs, *cur);
            cs.insert(e, '\n');
            *cur = e + 1;
            *changed = true;
            state.mode = VimMode::Insert;
        }
        'O' => {
            let s = line_start(cs, *cur);
            cs.insert(s, '\n');
            *cur = s;
            *changed = true;
            state.mode = VimMode::Insert;
        }
        'x' => {
            let e = (*cur + count).min(line_end(cs, *cur));
            if e > *cur {
                yank(state, cs, *cur, e, false);
                delete_range(cs, *cur, e, changed);
            }
        }
        'D' => {
            let e = line_end(cs, *cur);
            yank(state, cs, *cur, e, false);
            delete_range(cs, *cur, e, changed);
        }
        'C' => {
            let e = line_end(cs, *cur);
            yank(state, cs, *cur, e, false);
            delete_range(cs, *cur, e, changed);
            state.mode = VimMode::Insert;
        }
        'd' => state.pending = Some(Op::Delete),
        'c' => state.pending = Some(Op::Change),
        'y' => state.pending = Some(Op::Yank),
        'p' => paste(state, cs, cur, changed, true),
        'P' => paste(state, cs, cur, changed, false),
        'u' => out.undo = true,
        'v' => {
            state.mode = VimMode::Visual;
            state.visual_anchor = *cur;
        }
        'V' => {
            state.mode = VimMode::VisualLine;
            state.visual_anchor = *cur;
        }
        _ => {}
    }
}

/// Apply an operator (d/c/y) with the motion char that followed it.
fn operator_motion(
    state: &mut VimState,
    cs: &mut Vec<char>,
    cur: &mut usize,
    changed: &mut bool,
    op: Op,
    ch: char,
    count: usize,
) {
    // Doubled operator (dd/cc/yy): linewise on `count` lines.
    let doubled = matches!(
        (op, ch),
        (Op::Delete, 'd') | (Op::Change, 'c') | (Op::Yank, 'y')
    );
    let (a, b, linewise) = if doubled {
        let start = line_start(cs, *cur);
        let mut end = start;
        for _ in 0..count {
            end = line_end(cs, end);
            if end < cs.len() {
                end += 1; // include the newline
            } else {
                break;
            }
        }
        (start, end, true)
    } else {
        // cw behaves like ce (change to word end, inclusive).
        let mch = if op == Op::Change && ch == 'w' { 'e' } else { ch };
        let target = motion(cs, *cur, mch, count, state);
        let inclusive = matches!(mch, 'e' | '$');
        let (lo, hi) = ((*cur).min(target), (*cur).max(target));
        let hi = if inclusive { (hi + 1).min(cs.len()) } else { hi };
        (lo, hi, false)
    };

    yank(state, cs, a, b, linewise);
    match op {
        Op::Yank => {
            *cur = a;
        }
        Op::Delete => {
            delete_range(cs, a, b, changed);
            *cur = a;
        }
        Op::Change => {
            // Linewise change keeps the blank line.
            if linewise {
                let inner_end = b.saturating_sub(1).max(a);
                delete_range(cs, a, inner_end, changed);
            } else {
                delete_range(cs, a, b, changed);
            }
            *cur = a;
            state.mode = VimMode::Insert;
        }
    }
}

/// Compute the new cursor for a motion char from `start`.
fn motion(cs: &[char], start: usize, ch: char, count: usize, _state: &VimState) -> usize {
    let mut i = start;
    for _ in 0..count.max(1) {
        i = match ch {
            'h' => {
                let ls = line_start(cs, i);
                if i > ls { i - 1 } else { i }
            }
            'l' => {
                let le = line_end(cs, i);
                if i + 1 <= le && i < le { i + 1 } else { i }
            }
            'j' => move_line(cs, i, 1),
            'k' => move_line(cs, i, -1),
            '0' => line_start(cs, i),
            '^' => first_non_blank(cs, i),
            '$' => line_end(cs, i),
            'w' => next_word_start(cs, i),
            'b' => prev_word_start(cs, i),
            'e' => next_word_end(cs, i),
            _ => i,
        };
    }
    i
}

fn goto_last_or(cs: &[char], explicit: Option<usize>) -> usize {
    match explicit {
        Some(n) => nth_line_start(cs, n.saturating_sub(1)),
        None => {
            let last = cs.iter().filter(|&&c| c == '\n').count();
            nth_line_start(cs, last)
        }
    }
}

fn visual_range(state: &VimState, cur: usize, cs: &[char]) -> (usize, usize) {
    let (mut a, mut b) = (state.visual_anchor.min(cur), state.visual_anchor.max(cur));
    if state.mode == VimMode::VisualLine {
        a = line_start(cs, a);
        b = line_end(cs, b);
        if b < cs.len() {
            b += 1;
        }
    } else {
        // Char visual is inclusive of the char under the cursor.
        b = (b + 1).min(cs.len());
    }
    (a, b)
}

fn yank(state: &mut VimState, cs: &[char], a: usize, b: usize, linewise: bool) {
    state.register = cs[a.min(cs.len())..b.min(cs.len())].iter().collect();
    state.register_linewise = linewise;
}

fn paste(state: &VimState, cs: &mut Vec<char>, cur: &mut usize, changed: &mut bool, after: bool) {
    if state.register.is_empty() {
        return;
    }
    let reg: Vec<char> = state.register.chars().collect();
    let at = if state.register_linewise {
        if after {
            let e = line_end(cs, *cur);
            if e < cs.len() { e + 1 } else { cs.len() }
        } else {
            line_start(cs, *cur)
        }
    } else if after {
        (*cur + 1).min(cs.len())
    } else {
        *cur
    };
    let n = reg.len();
    cs.splice(at..at, reg);
    *cur = at + n.saturating_sub(1);
    *changed = true;
}

fn delete_range(cs: &mut Vec<char>, a: usize, b: usize, changed: &mut bool) {
    let a = a.min(cs.len());
    let b = b.min(cs.len());
    if b > a {
        cs.drain(a..b);
        *changed = true;
    }
}

fn clamp_on_char(cs: &[char], i: usize) -> usize {
    let ls = line_start(cs, i);
    let le = line_end(cs, i);
    if le > ls {
        i.clamp(ls, le - 1)
    } else {
        ls
    }
}

fn line_start(cs: &[char], i: usize) -> usize {
    let i = i.min(cs.len());
    let mut s = i;
    while s > 0 && cs[s - 1] != '\n' {
        s -= 1;
    }
    s
}

/// Index of the line's terminating '\n' (or end of text).
fn line_end(cs: &[char], i: usize) -> usize {
    let mut e = i.min(cs.len());
    while e < cs.len() && cs[e] != '\n' {
        e += 1;
    }
    e
}

fn first_non_blank(cs: &[char], i: usize) -> usize {
    let s = line_start(cs, i);
    let e = line_end(cs, i);
    let mut j = s;
    while j < e && cs[j].is_whitespace() {
        j += 1;
    }
    if j < e { j } else { s }
}

fn nth_line_start(cs: &[char], line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut seen = 0;
    for (idx, &c) in cs.iter().enumerate() {
        if c == '\n' {
            seen += 1;
            if seen == line {
                return idx + 1;
            }
        }
    }
    line_start(cs, cs.len())
}

fn move_line(cs: &[char], i: usize, delta: isize) -> usize {
    let col = i - line_start(cs, i);
    let target_start = if delta < 0 {
        let s = line_start(cs, i);
        if s == 0 {
            return i;
        }
        line_start(cs, s - 1)
    } else {
        let e = line_end(cs, i);
        if e >= cs.len() {
            return i;
        }
        e + 1
    };
    let target_end = line_end(cs, target_start);
    (target_start + col).min(target_end)
}

#[derive(PartialEq)]
enum Class {
    Word,
    Punct,
    Space,
}

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Punct
    }
}

fn next_word_start(cs: &[char], i: usize) -> usize {
    let n = cs.len();
    if i >= n {
        return n;
    }
    let mut j = i;
    let start_class = class(cs[j]);
    if start_class != Class::Space {
        while j < n && class(cs[j]) == start_class {
            j += 1;
        }
    }
    while j < n && class(cs[j]) == Class::Space {
        j += 1;
    }
    j.min(n)
}

fn prev_word_start(cs: &[char], i: usize) -> usize {
    if i == 0 {
        return 0;
    }
    let mut j = i - 1;
    while j > 0 && class(cs[j]) == Class::Space {
        j -= 1;
    }
    let c = class(cs[j]);
    while j > 0 && class(cs[j - 1]) == c {
        j -= 1;
    }
    j
}

fn next_word_end(cs: &[char], i: usize) -> usize {
    let n = cs.len();
    if i + 1 >= n {
        return i.min(n.saturating_sub(1));
    }
    let mut j = i + 1;
    while j < n && class(cs[j]) == Class::Space {
        j += 1;
    }
    if j >= n {
        return n - 1;
    }
    let c = class(cs[j]);
    while j + 1 < n && class(cs[j + 1]) == c {
        j += 1;
    }
    j
}
