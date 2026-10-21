//! Text in shapes and speaker notes as shared rich text, so several people can type in the same
//! text box at once.
//!
//! A text body's paragraphs become one Yjs text: each character carries its run's formatting as
//! attributes (`r`: the run properties as JSON, `k`: the run kind for line breaks, fields and
//! equations), and every paragraph ends with a `\n` whose `p` attribute holds the paragraph's own
//! settings (level, alignment, bullets…). Typing changes characters; formatting changes only
//! attributes, so someone making a word bold and someone typing in it both keep their change.
//!
//! Positions are counted like the engine counts them: one per character of a run's content
//! (a line break is `\u{b}`), one per paragraph end.

use std::sync::Arc;

use deckcraft_model::text::{Paragraph, Run, RunKind, RunProps};
use yrs::types::Attrs;
use yrs::types::text::{Diff, YChange};
use yrs::{Any, Assoc, GetString, IndexedSequence, Out, ReadTxn, StickyIndex, Text, TextRef, TransactionMut};

const R: &str = "r";
const K: &str = "k";
const P: &str = "p";

/// One position of the shared text: a character with its formatting, or a paragraph end.
#[derive(Clone, Debug, PartialEq)]
struct Unit {
    ch: char,
    /// Run properties (`None`: defaults).
    r: Option<Arc<str>>,
    /// Run kind (`None`: plain text).
    k: Option<Arc<str>>,
    /// Paragraph settings, on paragraph ends only.
    p: Option<Arc<str>>,
}

impl Unit {
    fn attrs(&self) -> Attrs {
        let v = |s: &Option<Arc<str>>| s.clone().map(Any::String).unwrap_or(Any::Null);
        Attrs::from([(Arc::from(R), v(&self.r)), (Arc::from(K), v(&self.k)), (Arc::from(P), v(&self.p))])
    }
    fn same_format(&self, other: &Unit) -> bool {
        self.r == other.r && self.k == other.k && self.p == other.p
    }
}

fn json<T: serde::Serialize>(v: &T) -> Option<Arc<str>> {
    serde_json::to_string(v).ok().map(Arc::from)
}

/// The paragraph's own settings (everything but its runs).
fn para_json(p: &Paragraph) -> Option<Arc<str>> {
    let mut q = p.clone();
    q.runs.clear();
    json(&q)
}

fn units(paragraphs: &[Paragraph]) -> Vec<Unit> {
    let mut out = Vec::new();
    for para in paragraphs {
        for run in &para.runs {
            let r = (run.props != RunProps::default()).then(|| json(&run.props)).flatten();
            let k = (run.kind != RunKind::Text).then(|| json(&run.kind)).flatten();
            for ch in run.content().chars() {
                // A paragraph end inside a run would split the paragraph for everyone else.
                let ch = if ch == '\n' { ' ' } else { ch };
                out.push(Unit { ch, r: r.clone(), k: k.clone(), p: None });
            }
        }
        out.push(Unit { ch: '\n', r: None, k: None, p: para_json(para) });
    }
    out
}

fn bytes(units: &[Unit]) -> u32 {
    units.iter().map(|u| u.ch.len_utf8() as u32).sum()
}

/// Edit the shared text from `old` into `new` with the fewest changes: the characters that
/// differ are replaced, and formatting that differs elsewhere is applied as formatting only.
pub(crate) fn write(txn: &mut TransactionMut, text: &TextRef, old: &[Paragraph], new: &[Paragraph]) {
    let a = units(old);
    let b = units(new);
    let pre = a.iter().zip(&b).take_while(|(x, y)| x.ch == y.ch).count();
    let max_suf = a.len().min(b.len()) - pre;
    let suf = a.iter().rev().zip(b.iter().rev()).take(max_suf).take_while(|(x, y)| x.ch == y.ch).count();
    let start = bytes(&b[..pre]);
    // Replace the middle.
    let removed = bytes(&a[pre..a.len() - suf]);
    if removed > 0 {
        text.remove_range(txn, start, removed);
    }
    let mut at = start;
    let middle = &b[pre..b.len() - suf];
    let mut i = 0;
    while i < middle.len() {
        let mut j = i + 1;
        while j < middle.len() && middle[j].same_format(&middle[i]) {
            j += 1;
        }
        let chunk: String = middle[i..j].iter().map(|u| u.ch).collect();
        text.insert_with_attributes(txn, at, &chunk, middle[i].attrs());
        at += chunk.len() as u32;
        i = j;
    }
    // Formatting changes in the parts that kept their characters.
    let mut format = |units_old: &[Unit], units_new: &[Unit], mut pos: u32| {
        let mut i = 0;
        while i < units_new.len() {
            if units_old[i].same_format(&units_new[i]) {
                pos += units_new[i].ch.len_utf8() as u32;
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < units_new.len() && !units_old[j].same_format(&units_new[j]) && units_new[j].same_format(&units_new[i]) {
                j += 1;
            }
            let len = bytes(&units_new[i..j]);
            text.format(txn, pos, len, units_new[i].attrs());
            pos += len;
            i = j;
        }
    };
    format(&a[..pre], &b[..pre], 0);
    format(&a[a.len() - suf..], &b[b.len() - suf..], at);
}

/// The paragraphs the shared text describes.
pub(crate) fn read<T: ReadTxn>(txn: &T, text: &TextRef) -> Vec<Paragraph> {
    let chunks: Vec<Diff<YChange>> = text.diff(txn, YChange::identity);
    let mut paragraphs = Vec::new();
    let mut para = Paragraph::default();
    let attr = |a: &Option<Box<Attrs>>, key: &str| -> Option<Arc<str>> {
        match a.as_ref()?.get(key)? {
            Any::String(s) => Some(s.clone()),
            _ => None,
        }
    };
    for chunk in chunks {
        let Out::Any(Any::String(s)) = &chunk.insert else { continue };
        let r = attr(&chunk.attributes, R);
        let k = attr(&chunk.attributes, K);
        let p = attr(&chunk.attributes, P);
        let props: RunProps = r.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
        let kind: RunKind = k.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or(RunKind::Text);
        for ch in s.chars() {
            if ch == '\n' {
                let mut done: Paragraph = p.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
                done.runs = std::mem::take(&mut para.runs);
                paragraphs.push(done);
                continue;
            }
            push_char(&mut para.runs, ch, &props, &kind);
        }
    }
    // Text after the last paragraph end (someone deleted it): one more paragraph.
    if !para.runs.is_empty() {
        paragraphs.push(para);
    }
    paragraphs
}

fn push_char(runs: &mut Vec<Run>, ch: char, props: &RunProps, kind: &RunKind) {
    // A line break is a run of its own; other characters join the run before them when it has
    // the same formatting and kind.
    if *kind != RunKind::Break
        && let Some(last) = runs.last_mut()
        && last.kind == *kind
        && last.props == *props
    {
        last.text.push(ch);
        return;
    }
    let text = if *kind == RunKind::Break { String::new() } else { ch.to_string() };
    runs.push(Run { text, props: props.clone(), kind: kind.clone() });
}

/// The index in the shared text (bytes) of `(paragraph, char)` as the engine counts it.
pub(crate) fn index_of<T: ReadTxn>(txn: &T, text: &TextRef, pos: (usize, usize)) -> u32 {
    let s = text.get_string(txn);
    let mut para = 0;
    let mut chars = 0;
    for (i, ch) in s.char_indices() {
        if para == pos.0 && chars == pos.1 {
            return i as u32;
        }
        if ch == '\n' {
            if para == pos.0 {
                return i as u32;
            }
            para += 1;
            chars = 0;
        } else if para == pos.0 {
            chars += 1;
        }
    }
    s.len() as u32
}

/// `(paragraph, char)` of a shared text index.
pub(crate) fn pos_of<T: ReadTxn>(txn: &T, text: &TextRef, index: u32) -> (usize, usize) {
    let s = text.get_string(txn);
    let mut para = 0;
    let mut chars = 0;
    for (i, ch) in s.char_indices() {
        if i as u32 >= index {
            break;
        }
        if ch == '\n' {
            para += 1;
            chars = 0;
        } else {
            chars += 1;
        }
    }
    // Past the last paragraph end there is no paragraph: stay at the end of the last one.
    if s.ends_with('\n') && index as usize >= s.len() && para > 0 {
        let last = s[..s.len() - 1].rsplit('\n').next().map(|l| l.chars().count()).unwrap_or(0);
        return (para - 1, last);
    }
    (para, chars)
}

/// A position that moves with the text when others type before it.
pub(crate) fn anchor<T: ReadTxn>(txn: &T, text: &TextRef, pos: (usize, usize)) -> Option<StickyIndex> {
    text.sticky_index(txn, index_of(txn, text, pos), Assoc::After)
}

pub(crate) fn resolve<T: ReadTxn>(txn: &T, text: &TextRef, sticky: &StickyIndex) -> Option<(usize, usize)> {
    let offset = sticky.get_offset(txn)?;
    Some(pos_of(txn, text, offset.index))
}
