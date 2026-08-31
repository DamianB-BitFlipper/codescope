//! Intraline (delta-style) change highlighting for the diff pane.
//!
//! Within a paired removed/added line, find the changed *word* spans on each side so the
//! renderer can brighten just those, leaving the rest of the line in the normal add/del
//! style. Pairing groups a maximal run of `Del` rows followed by a maximal run of `Add`
//! rows (a modification block) and pairs them positionally; unpaired extras get no spans.
//!
//! Pure: no ratatui, no I/O. Spans are byte ranges into the line text, sorted,
//! non-overlapping, and merged when adjacent; the renderer maps them onto graphemes
//! before styling, so span boundaries can never split a grapheme cluster.

use similar::{ChangeTag, TextDiff};

use crate::snapshot::DiffRow;

/// A modification block (del run + add run) with more lines than this is not paired:
/// past a point positional pairing produces noise, and the word diff is wasted work.
pub const MAX_BLOCK_LINES: usize = 40;

/// A line longer than this (in chars) gets no intraline spans: the word diff is
/// superlinear and a minified line would stall a render pass.
pub const MAX_LINE_CHARS: usize = 2000;

/// Byte spans of the changed words on one side of a line pair: sorted,
/// non-overlapping, adjacent-merged ranges into that side's line text.
pub type ByteSpans = Vec<(usize, usize)>;

/// The kind of a diff row, for block detection in [`pair_rows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// `@@` hunk header (breaks a block).
    Header,
    /// Context line (breaks a block).
    Context,
    /// `-` removed line.
    Del,
    /// `+` added line.
    Add,
}

/// Pair rows inside each modification block positionally.
///
/// Returns, per row index, the index of its intraline partner (`Del` i ↔ `Add` j).
/// A block is a maximal `Del` run immediately followed by a maximal `Add` run; context
/// lines, hunk headers, and a `Del` that follows an `Add` all close the block. Both
/// sides must be non-empty, the block must fit [`MAX_BLOCK_LINES`], and only the first
/// `min(dels, adds)` pairs are matched — unpaired extras get `None`.
pub fn pair_rows(kinds: &[RowKind]) -> Vec<Option<usize>> {
    let mut pairs = vec![None; kinds.len()];
    let mut dels: Vec<usize> = Vec::new();
    let mut adds: Vec<usize> = Vec::new();
    for (i, kind) in kinds.iter().enumerate() {
        match kind {
            RowKind::Del => {
                // A Del after Adds opens a new block (`Del Add Del`): close the old one.
                if !adds.is_empty() {
                    pair_block(&mut pairs, &dels, &adds);
                    dels.clear();
                    adds.clear();
                }
                dels.push(i);
            }
            RowKind::Add => adds.push(i),
            RowKind::Header | RowKind::Context => {
                pair_block(&mut pairs, &dels, &adds);
                dels.clear();
                adds.clear();
            }
        }
    }
    pair_block(&mut pairs, &dels, &adds);
    pairs
}

/// Changed word spans on each side of a removed/added line pair.
///
/// Returns `(old_spans, new_spans)` as byte ranges into `old` / `new`. Words, not chars:
/// a renamed identifier highlights whole, not letter-by-letter. Adjacent changed tokens
/// merge into one span. Both sides come back empty when the lines are identical, one
/// exceeds [`MAX_LINE_CHARS`], or the pair shares no equal *word* token — two unrelated
/// replacement lines would otherwise light up as one giant bright block, the opposite
/// of what intraline highlighting is for (docs/review/15 §3.4 step 3).
pub fn changed_spans(old: &str, new: &str) -> (ByteSpans, ByteSpans) {
    if old.chars().count() > MAX_LINE_CHARS || new.chars().count() > MAX_LINE_CHARS {
        return (Vec::new(), Vec::new());
    }
    let old_tokens = tokenize_words(old);
    let new_tokens = tokenize_words(new);
    let diff = TextDiff::from_slices(&old_tokens, &new_tokens);
    let mut old_spans: Vec<(usize, usize)> = Vec::new();
    let mut new_spans: Vec<(usize, usize)> = Vec::new();
    let mut old_pos = 0usize;
    let mut new_pos = 0usize;
    let mut shared_word = false;
    // `iter_all_changes` walks tokens in order and emits Replace ops as old-side Deletes
    // followed by new-side Inserts, so the byte cursors stay in sync with the input.
    for change in diff.iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                // Punctuation runs (`(`, `==`) survive most edits; only a shared word
                // proves the two lines are one statement that changed, not two
                // unrelated statements that happen to sit at the same spot.
                shared_word |= matches!(classify_token(change.value()), Class::Word);
                old_pos += len;
                new_pos += len;
            }
            ChangeTag::Delete => {
                push_span(&mut old_spans, old_pos, old_pos + len);
                old_pos += len;
            }
            ChangeTag::Insert => {
                push_span(&mut new_spans, new_pos, new_pos + len);
                new_pos += len;
            }
        }
    }
    if !shared_word {
        return (Vec::new(), Vec::new());
    }
    (old_spans, new_spans)
}

/// Per-row changed byte spans for the renderer (empty when the row is unpaired or under
/// a cap). Each pair is diffed once, from the `Del` side.
/// Pair del/add rows within each modification block by *content alignment*: a
/// `TextDiff::from_lines` over the block's del texts vs add texts, zipping only the two
/// sides of each `Replace` op. Positional pairing mis-pairs everything below an insertion
/// inside the run (review 16 M4): with `["let keep = 1", "return old"]` vs
/// `["let inserted = 0", "let keep = 1", "return new"]`, the two `return` lines pair and
/// the pure insertion stays unpaired.
fn pair_rows_aligned(rows: &[DiffRow]) -> Vec<Option<usize>> {
    let kinds: Vec<RowKind> = rows.iter().map(row_kind).collect();
    let mut pairs = vec![None; rows.len()];
    let mut dels: Vec<usize> = Vec::new();
    let mut adds: Vec<usize> = Vec::new();
    let flush = |pairs: &mut Vec<Option<usize>>, dels: &mut Vec<usize>, adds: &mut Vec<usize>| {
        if !dels.is_empty() && !adds.is_empty() && dels.len() + adds.len() <= MAX_BLOCK_LINES {
            let old_joined = dels.iter().map(|&i| row_text(&rows[i])).collect::<Vec<_>>().join("\n");
            let new_joined = adds.iter().map(|&i| row_text(&rows[i])).collect::<Vec<_>>().join("\n");
            let diff = TextDiff::from_lines(&old_joined, &new_joined);
            // Group consecutive changes by op class; zip only the two sides of a Replace
            // group (a run of deletes followed by a run of inserts with no equal between).
            let mut d = 0usize;
            let mut a = 0usize;
            let mut pending_del: Vec<usize> = Vec::new();
            let mut pending_add: Vec<usize> = Vec::new();
            let flush_group = |pairs: &mut Vec<Option<usize>>, pd: &mut Vec<usize>, pa: &mut Vec<usize>| {
                for k in 0..pd.len().min(pa.len()) {
                    pairs[dels[pd[k]]] = Some(adds[pa[k]]);
                    pairs[adds[pa[k]]] = Some(dels[pd[k]]);
                }
                pd.clear();
                pa.clear();
            };
            for change in diff.iter_all_changes() {
                match change.tag() {
                    ChangeTag::Delete => {
                        pending_del.push(d);
                        d += 1;
                    }
                    ChangeTag::Insert => {
                        pending_add.push(a);
                        a += 1;
                    }
                    ChangeTag::Equal => {
                        // Sequences re-converged: close the pending replace group; the
                        // equal lines themselves are unchanged content (no highlight).
                        flush_group(pairs, &mut pending_del, &mut pending_add);
                        d += 1;
                        a += 1;
                    }
                }
            }
            flush_group(pairs, &mut pending_del, &mut pending_add);
        }
        dels.clear();
        adds.clear();
    };
    for (i, kind) in kinds.iter().enumerate() {
        match kind {
            RowKind::Del => {
                if !adds.is_empty() {
                    flush(&mut pairs, &mut dels, &mut adds);
                }
                dels.push(i);
            }
            RowKind::Add => adds.push(i),
            RowKind::Header | RowKind::Context => flush(&mut pairs, &mut dels, &mut adds),
        }
    }
    flush(&mut pairs, &mut dels, &mut adds);
    pairs
}

/// Per-row intraline change spans: one entry per row; non-empty on the rows of a
/// content-paired del/add line.
pub fn row_spans(rows: &[DiffRow]) -> Vec<ByteSpans> {
    let pairs = pair_rows_aligned(rows);
    let mut spans: Vec<ByteSpans> = vec![Vec::new(); rows.len()];
    for (i, partner) in pairs.iter().enumerate() {
        let Some(j) = partner else { continue };
        // Blocks list Dels before Adds, so the smaller index of a pair is the Del side;
        // the Add side is filled when its Del partner is visited.
        if *j < i {
            continue;
        }
        let (old_spans, new_spans) = changed_spans(row_text(&rows[i]), row_text(&rows[*j]));
        spans[i] = old_spans;
        spans[*j] = new_spans;
    }
    spans
}

/// The pairing kind of a snapshot row.
fn row_kind(row: &DiffRow) -> RowKind {
    match row {
        DiffRow::HunkHeader(_) => RowKind::Header,
        DiffRow::Context { .. } => RowKind::Context,
        DiffRow::Del { .. } => RowKind::Del,
        DiffRow::Add { .. } => RowKind::Add,
    }
}

/// The source text of a diff row (headers count as their full text; unused for spans).
fn row_text(row: &DiffRow) -> &str {
    match row {
        DiffRow::HunkHeader(h) => h,
        DiffRow::Add { text, .. } | DiffRow::Del { text, .. } | DiffRow::Context { text, .. } => {
            text
        }
    }
}

/// Pair one modification block positionally; oversized or one-sided blocks stay unpaired.
fn pair_block(pairs: &mut [Option<usize>], dels: &[usize], adds: &[usize]) {
    if dels.is_empty() || adds.is_empty() || dels.len() + adds.len() > MAX_BLOCK_LINES {
        return;
    }
    for (d, a) in dels.iter().zip(adds.iter()) {
        pairs[*d] = Some(*a);
        pairs[*a] = Some(*d);
    }
}

/// Append `[start, end)` to `spans`, merging with the previous span when they touch:
/// adjacent changed tokens (a word and its trailing punctuation replaced together) read
/// as one highlight.
fn push_span(spans: &mut Vec<(usize, usize)>, start: usize, end: usize) {
    match spans.last_mut() {
        Some(last) if last.1 == start => last.1 = end,
        _ => spans.push((start, end)),
    }
}

/// Token class for [`tokenize_words`]: word chars (`is_alphanumeric` or `_`),
/// whitespace, or anything else (punctuation).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn classify(c: char) -> Class {
    if c == '_' || c.is_alphanumeric() {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// The class of a whole token (tokens are maximal same-class runs, so this reads the
/// first char). Guards the unrelated-line check in [`changed_spans`].
fn classify_token(t: &str) -> Class {
    classify(t.chars().next().unwrap_or(' '))
}

/// Split `s` into maximal same-class runs. Tokens concatenate back to `s` exactly, so
/// byte offsets accumulate losslessly. Splitting words from punctuation (`foo(1)` →
/// `foo` `(` `1` `)`) keeps a one-argument edit from highlighting the whole call.
fn tokenize_words(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut prev: Option<Class> = None;
    for (i, c) in s.char_indices() {
        let class = classify(c);
        if prev.is_some_and(|p| p != class) {
            out.push(&s[start..i]);
            start = i;
        }
        prev = Some(class);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compact pairing specs: `H` header, `C` context, `D` del, `A` add; `.` ignored.
    fn kinds(spec: &str) -> Vec<RowKind> {
        spec.chars()
            .filter_map(|c| match c {
                'H' => Some(RowKind::Header),
                'C' => Some(RowKind::Context),
                'D' => Some(RowKind::Del),
                'A' => Some(RowKind::Add),
                _ => None,
            })
            .collect()
    }

    // -- pairing -----------------------------------------------------------

    #[test]
    fn pair_equal_block() {
        assert_eq!(
            pair_rows(&kinds("H DD AA")),
            vec![None, Some(3), Some(4), Some(1), Some(2)]
        );
    }

    #[test]
    fn pair_unequal_block_leaves_extras_unpaired() {
        // 3 dels, 1 add: only the first del pairs.
        assert_eq!(
            pair_rows(&kinds("DDDA")),
            vec![Some(3), None, None, Some(0)]
        );
        // 1 del, 3 adds: only the first add pairs.
        assert_eq!(
            pair_rows(&kinds("DAAA")),
            vec![Some(1), Some(0), None, None]
        );
    }

    #[test]
    fn pair_pure_add_and_pure_del_stay_unpaired() {
        assert_eq!(pair_rows(&kinds("H AAA")), vec![None; 4]);
        assert_eq!(pair_rows(&kinds("DDD")), vec![None; 3]);
    }

    #[test]
    fn pair_blocks_split_on_context_and_headers() {
        // Context between the runs: no block spans it.
        assert_eq!(pair_rows(&kinds("D C A")), vec![None; 3]);
        // Two blocks separated by a header pair independently.
        assert_eq!(
            pair_rows(&kinds("DA H DA")),
            vec![Some(1), Some(0), None, Some(4), Some(3)]
        );
        // A Del after an Add opens a new block instead of extending the old one.
        assert_eq!(
            pair_rows(&kinds("DA D A")),
            vec![Some(1), Some(0), Some(3), Some(2)]
        );
    }

    #[test]
    fn pair_block_cap() {
        // 20 + 20 = 40 lines: still paired.
        let ok = format!("{} {}", "D".repeat(20), "A".repeat(20));
        let pairs = pair_rows(&kinds(&ok));
        assert_eq!(pairs[0], Some(20));
        assert_eq!(pairs[19], Some(39));
        assert_eq!(pairs[20], Some(0));
        // 21 + 21 = 42 lines: over the cap, nothing pairs.
        let big = format!("{} {}", "D".repeat(21), "A".repeat(21));
        assert_eq!(pair_rows(&kinds(&big)), vec![None; 42]);
        // One side alone over the cap sinks the whole block.
        let lopsided = format!("{} {}", "D".repeat(41), "A");
        assert_eq!(pair_rows(&kinds(&lopsided)), vec![None; 42]);
    }

    // -- span computation ---------------------------------------------------

    #[test]
    fn changed_word_in_the_middle() {
        let old = "    let timeout = 30;";
        let new = "    let timeout = 60;";
        let (old_spans, new_spans) = changed_spans(old, new);
        assert_eq!(old_spans, vec![(old.find("30").unwrap(), old.find("30").unwrap() + 2)]);
        assert_eq!(new_spans, vec![(new.find("60").unwrap(), new.find("60").unwrap() + 2)]);
    }

    #[test]
    fn changed_word_at_start_and_end() {
        let (old_spans, new_spans) = changed_spans("foo = 1", "bar = 1");
        assert_eq!(old_spans, vec![(0, 3)]);
        assert_eq!(new_spans, vec![(0, 3)]);

        let (old_spans, new_spans) = changed_spans("x = old", "x = new_longer");
        assert_eq!(old_spans, vec![(4, 7)]);
        assert_eq!(new_spans, vec![(4, 14)]);
    }

    #[test]
    fn changed_whole_line() {
        // Every word rewritten but the shape survives: the shared `=` is not a word
        // token, but `let`/`return`-style anchors are. Here "aaaa"→"bbbb" shares no
        // word at all, so both sides stay dark (unrelated-line guard).
        assert_eq!(changed_spans("aaaa", "bbbb"), (vec![], vec![]));
        // Same shape, different content, anchored by a shared word: whole content
        // highlights.
        let (old_spans, new_spans) = changed_spans("let x = aaaa", "let x = bbbb");
        assert_eq!(old_spans, vec![(8, 12)]);
        assert_eq!(new_spans, vec![(8, 12)]);
    }

    #[test]
    fn unrelated_replacement_lines_stay_dark() {
        // Punctuation survives (`==`, `(`) but no word token does: highlighting here
        // would be one giant bright block (docs/review/15 §3.4 step 3).
        assert_eq!(
            changed_spans("if a == b {", "for x in xs() {"),
            (vec![], vec![])
        );
        assert_eq!(changed_spans("return foo(1)", "break bar(2)"), (vec![], vec![]));
    }

    #[test]
    fn changed_scattered_words_stay_separate_spans() {
        // Two changed words with unchanged text between them: two spans per side.
        let old = "aaa bbb ccc";
        let new = "xxx bbb yyy";
        let (old_spans, new_spans) = changed_spans(old, new);
        assert_eq!(old_spans, vec![(0, 3), (8, 11)]);
        assert_eq!(new_spans, vec![(0, 3), (8, 11)]);
    }

    #[test]
    fn changed_identifier_inside_punctuation() {
        // Word/punctuation token split: only the argument lights up, not the call.
        let (old_spans, new_spans) = changed_spans("foo(1);", "foo(2);");
        assert_eq!(old_spans, vec![(4, 5)]);
        assert_eq!(new_spans, vec![(4, 5)]);
    }

    #[test]
    fn changed_adjacent_tokens_merge() {
        // "30;" replaced by a longer token run: touching changed tokens read as one span.
        let (old_spans, new_spans) = changed_spans("x = 30;", "x = 60 + 2;");
        assert_eq!(old_spans, vec![(4, 6)], "just `30`; the trailing `;` survives");
        assert_eq!(new_spans, vec![(4, 10)], "`60 + 2` merges into one span");
    }

    #[test]
    fn identical_lines_have_no_spans() {
        assert_eq!(changed_spans("same line", "same line"), (vec![], vec![]));
        assert_eq!(changed_spans("", ""), (vec![], vec![]));
    }

    #[test]
    fn changed_unicode_uses_byte_offsets() {
        let old = "héllo wörld = 1";
        let new = "héllo wörld = 2";
        let (old_spans, new_spans) = changed_spans(old, new);
        assert_eq!(old_spans.len(), 1);
        let (s, e) = old_spans[0];
        assert_eq!(&old[s..e], "1");
        assert_eq!(new_spans.len(), 1);
        let (s, e) = new_spans[0];
        assert_eq!(&new[s..e], "2");
    }

    #[test]
    fn changed_line_cap() {
        let long_a = "a".repeat(MAX_LINE_CHARS + 1);
        let long_b = "b".repeat(MAX_LINE_CHARS + 1);
        assert_eq!(changed_spans(&long_a, &long_b), (vec![], vec![]));
        // One side over the cap is enough.
        assert_eq!(changed_spans("x", &long_b), (vec![], vec![]));
        // Exactly at the cap still diffs (with a shared anchor word so the
        // unrelated-line guard stays out of the way).
        let at_a = format!("let {}", "a".repeat(MAX_LINE_CHARS - 4));
        let at_b = format!("let {}", "b".repeat(MAX_LINE_CHARS - 4));
        let (old_spans, new_spans) = changed_spans(&at_a, &at_b);
        assert_eq!(old_spans, vec![(4, MAX_LINE_CHARS)]);
        assert_eq!(new_spans, vec![(4, MAX_LINE_CHARS)]);
    }

    // -- row orchestration ----------------------------------------------------

    #[test]
    fn row_spans_hit_only_paired_rows() {
        let rows = vec![
            DiffRow::HunkHeader("@@ -1,3 +1,3 @@".to_string()),
            DiffRow::Context {
                old_ln: 1,
                new_ln: 1,
                text: "fn main() {".to_string(),
            },
            DiffRow::Del {
                old_ln: 2,
                text: "    let timeout = 30;".to_string(),
            },
            DiffRow::Add {
                new_ln: 2,
                text: "    let timeout = 60;".to_string(),
            },
            DiffRow::Add {
                new_ln: 3,
                text: "    println!(\"done\");".to_string(),
            },
        ];
        let spans = row_spans(&rows);
        assert_eq!(spans.len(), rows.len());
        assert!(spans[0].is_empty(), "header: no spans");
        assert!(spans[1].is_empty(), "context: no spans");
        assert_eq!(spans[2], vec![(18, 20)], "del side highlights `30`");
        assert_eq!(spans[3], vec![(18, 20)], "add side highlights `60`");
        assert!(spans[4].is_empty(), "unpaired extra add: no spans");
    }


    /// Review 16 M4: an insertion inside a del/add run must not shift later partners.
    /// The two `return` lines pair; the pure insertion does not.
    #[test]
    fn inserted_line_does_not_shift_later_pairs() {
        let rows = vec![
            DiffRow::Del { old_ln: 1, text: "let keep = 1;".to_string() },
            DiffRow::Del { old_ln: 2, text: "return old".to_string() },
            DiffRow::Add { new_ln: 1, text: "let inserted = 0;".to_string() },
            DiffRow::Add { new_ln: 2, text: "let keep = 1;".to_string() },
            DiffRow::Add { new_ln: 3, text: "return new".to_string() },
        ];
        let pairs = pair_rows_aligned(&rows);
        assert_eq!(pairs[0], None, "pure delete of `let keep` (moved, not changed)");
        assert_eq!(pairs[1], Some(4), "`return old` pairs with `return new`");
        assert_eq!(pairs[4], Some(1));
        assert_eq!(pairs[2], None, "pure insertion unpaired");
        assert_eq!(pairs[3], None);
        let spans = row_spans(&rows);
        assert!(!spans[1].is_empty() && !spans[4].is_empty(), "old/new brighten");
        assert!(spans[2].is_empty(), "inserted line has no intraline spans");
    }
}
