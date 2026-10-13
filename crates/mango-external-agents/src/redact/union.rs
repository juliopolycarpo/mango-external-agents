//! Spans of a text that are replaced, and the union of what several readers want replaced.
//!
//! The redactor reads a text with more than one set of rules: the rules of the release before this
//! one (`redact::floor`), the reads added since (escape depth, containers across lines, sequences,
//! block scalars), the well-known tokens and, for a JSON text inside a string, the structure. Each
//! reader looks at the same stripped text and says which spans it replaces, in that text's own
//! coordinates, as a list of [`Hide`]. The union of the lists is applied once. A reader that
//! replaces less than another therefore cannot make the answer replace less than that other: the
//! answer hides every span any of them hides, whatever order they ran in, because none of them
//! sees the text another has changed.
//!
//! The rules of the floor are three passes, each over the text the one before left. [`compose`]
//! turns the lists of passes that follow one another into the one list over the text the first of
//! them read, which is what the union needs.

use std::borrow::Cow;

use super::strip::{self, remove_boundaries};
use super::{REDACTED, floor, tokens};

/// A span `start..end` of a text that is replaced by `lead` followed by `tail`.
///
/// What stays in front of the span (a credential's name) is not part of it. `lead` is what the
/// rules write for a redaction (`=[REDACTED]`, `[REDACTED]`, or nothing for a byte that is taken
/// out); `tail` is what follows the marker, the closing quote of a value whose glued text was
/// redacted with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Hide {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) lead: Cow<'static, str>,
    pub(super) tail: Cow<'static, str>,
}

impl Hide {
    /// A span replaced by the marker alone.
    pub(super) fn marker(start: usize, end: usize) -> Self {
        Self {
            start,
            end,
            lead: Cow::Borrowed(REDACTED),
            tail: Cow::Borrowed(""),
        }
    }

    /// A span written as `joiner`, the marker and `tail`: what a rule's match becomes.
    pub(super) fn written(
        start: usize,
        end: usize,
        joiner: &'static str,
        tail: &'static str,
    ) -> Self {
        let lead = if joiner.is_empty() {
            Cow::Borrowed(REDACTED)
        } else {
            Cow::Owned(format!("{joiner}{REDACTED}"))
        };
        Self {
            start,
            end,
            lead,
            tail: Cow::Borrowed(tail),
        }
    }

    /// A byte or a run of bytes taken out of the text.
    pub(super) fn taken_out(start: usize, end: usize) -> Self {
        Self {
            start,
            end,
            lead: Cow::Borrowed(""),
            tail: Cow::Borrowed(""),
        }
    }

    fn written_len(&self) -> usize {
        self.lead.len() + self.tail.len()
    }
}

/// `text[from..to]`, with a cut character repaired as the rules always repaired one.
fn slice(text: &str, from: usize, to: usize) -> Cow<'_, str> {
    match text.get(from..to) {
        Some(part) => Cow::Borrowed(part),
        None => String::from_utf8_lossy(text.as_bytes().get(from..to).unwrap_or_default()),
    }
}

/// `text` with every span of `hides` replaced. The spans are in order and do not overlap.
///
/// # Example
///
/// ```ignore
/// let hides = [Hide::written(3, 8, "=", "")];
/// assert_eq!(apply("key: abcd", &hides), "key=[REDACTED]d");
/// ```
pub(super) fn apply(text: &str, hides: &[Hide]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for hide in hides {
        out.push_str(&slice(text, copied, hide.start));
        out.push_str(&hide.lead);
        out.push_str(&hide.tail);
        copied = hide.end;
    }
    out.push_str(&slice(text, copied, text.len()));
    out
}

/// The spans of `next`, which read the text `middle` that `prev` made of its own, as spans of the
/// text `prev` read.
///
/// A span of `next` that starts or ends inside what `prev` wrote (inside its marker) takes the whole
/// of that write with it: what `prev` hid stays hidden and the rest of its marker is kept in the
/// text, so the spans, applied to the first text, give the text the two passes gave one after the
/// other. Both lists are in order and do not overlap.
///
/// # Example
///
/// ```ignore
/// // `prev` writes `=[REDACTED]` over bytes 3..8 of `key: abcd`; `next` takes the `D` out of it.
/// let both = compose(prev, "key=[REDACTED]d", next);
/// assert_eq!(apply("key: abcd", &both), apply("key=[REDACTED]d", &next));
/// ```
pub(super) fn compose(prev: Vec<Hide>, middle: &str, next: Vec<Hide>) -> Vec<Hide> {
    if next.is_empty() {
        return prev;
    }
    if prev.is_empty() {
        return next;
    }
    let placed = place(&prev);
    let mut groups: Vec<Group<'_>> = Vec::new();
    for edit in &next {
        let from = widen(&placed, edit.start, Side::Start);
        let to = widen(&placed, edit.end, Side::End);
        match groups.last_mut() {
            Some(group) if from < group.to => {
                group.to = group.to.max(to);
                group.members.push(edit);
            }
            _ => groups.push(Group {
                from,
                to,
                members: vec![edit],
            }),
        }
    }
    let mut dropped = vec![false; prev.len()];
    let mut out = Vec::with_capacity(prev.len() + groups.len());
    for group in &groups {
        let first = placed.partition_point(|span| span.to <= group.from);
        let last = placed.partition_point(|span| span.from < group.to);
        dropped[first..last.max(first)].fill(true);
        out.push(group.written(middle, &placed));
    }
    out.extend(
        prev.into_iter()
            .zip(dropped)
            .filter_map(|(hide, gone)| (!gone).then_some(hide)),
    );
    out.sort_by_key(|hide| (hide.start, hide.end));
    out
}

/// Where a span of the previous pass stands in the text it wrote, and how much shorter or longer
/// that text is than the one it read, counting this span and all before it.
#[derive(Clone, Copy)]
struct Placed {
    from: usize,
    to: usize,
    delta: isize,
}

/// A run of spans of the next pass that touch what the previous pass wrote at the same place, as
/// the span of the written text they cover.
struct Group<'a> {
    from: usize,
    to: usize,
    members: Vec<&'a Hide>,
}

#[derive(Clone, Copy)]
enum Side {
    Start,
    End,
}

fn place(prev: &[Hide]) -> Vec<Placed> {
    let mut delta = 0isize;
    prev.iter()
        .map(|hide| {
            let from = hide.start.saturating_add_signed(delta);
            let to = from + hide.written_len();
            delta += hide.written_len() as isize - (hide.end - hide.start) as isize;
            Placed { from, to, delta }
        })
        .collect()
}

/// `at`, moved to the edge of what the previous pass wrote if it falls inside it.
fn widen(placed: &[Placed], at: usize, side: Side) -> usize {
    let after = placed.partition_point(|span| span.to <= at);
    match placed.get(after) {
        Some(span) if span.from < at => match side {
            Side::Start => span.from,
            Side::End => span.to,
        },
        _ => at,
    }
}

impl Group<'_> {
    /// The group as a span of the text the previous pass read.
    fn written(&self, middle: &str, placed: &[Placed]) -> Hide {
        let start = unshifted(placed, self.from, Side::Start);
        let end = unshifted(placed, self.to, Side::End);
        let mut lead = String::new();
        let mut read = self.from;
        let last = self.members.len() - 1;
        for (index, member) in self.members.iter().enumerate() {
            lead.push_str(&slice(middle, read, member.start));
            lead.push_str(&member.lead);
            if index < last {
                lead.push_str(&member.tail);
            }
            read = member.end;
        }
        let tail_member = self.members[last];
        let mut tail = String::from(&*tail_member.tail);
        tail.push_str(&slice(middle, tail_member.end, self.to));
        Hide {
            start,
            end,
            lead: Cow::Owned(lead),
            tail: Cow::Owned(tail),
        }
    }
}

/// The position in the text the previous pass read that `at`, in the text it wrote, stands for.
/// A span that takes bytes out sits between the two positions it could be said to stand at: a span
/// that starts there is after it, and one that ends there is before it.
fn unshifted(placed: &[Placed], at: usize, side: Side) -> usize {
    let before = match side {
        Side::Start => placed.partition_point(|span| span.to <= at),
        Side::End => {
            placed.partition_point(|span| span.to < at || (span.to == at && span.from < at))
        }
    };
    let delta = before.checked_sub(1).map_or(0, |last| placed[last].delta);
    at.saturating_add_signed(-delta)
}

/// Every span any of the lists hides, as one list: spans that overlap become one span.
///
/// The merged span starts where the first starts and ends where the last ends. It is written as the
/// one that starts first writes it, so that a text no reader but the first wanted changed comes
/// out as that reader wrote it, and it closes as the one that ends last closes it. Among spans
/// that start or end together the earlier list wins.
///
/// # Example
///
/// ```ignore
/// let both = union(vec![vec![Hide::written(3, 8, "=", "")], vec![Hide::marker(5, 12)]]);
/// assert_eq!(both, vec![Hide::written(3, 12, "=", "")]);
/// ```
pub(super) fn union(lists: Vec<Vec<Hide>>) -> Vec<Hide> {
    let mut all: Vec<(usize, Hide)> = lists
        .into_iter()
        .enumerate()
        .flat_map(|(rank, list)| list.into_iter().map(move |hide| (rank, hide)))
        .collect();
    all.sort_by_key(|(rank, hide)| (hide.start, *rank));
    let mut out: Vec<Hide> = Vec::with_capacity(all.len());
    let mut tail_rank = 0usize;
    for (rank, hide) in all {
        let Some(open) = out.last_mut().filter(|open| hide.start < open.end) else {
            tail_rank = rank;
            out.push(hide);
            continue;
        };
        // A span that only takes bytes out leaves no marker, so the marker of the one it meets is
        // the one the merged span is written with.
        if open.lead.is_empty() && !hide.lead.is_empty() {
            open.lead = hide.lead.clone();
        }
        if hide.end > open.end || (hide.end == open.end && rank < tail_rank) {
            open.end = hide.end;
            open.tail = hide.tail;
            tail_rank = rank;
        }
    }
    out
}

/// What a caller adds to the readers of a text: a reader that knows a structure the text is
/// written in.
pub(super) trait Layer {
    /// The spans to hide, given the spans of the rules of the release before (`floor`) and the
    /// union of every other reader's (`others`), over the stripped text, and the layer's own.
    ///
    /// The floor is what a text is never shown without; the layer may widen a span of it and never
    /// narrow one. It may narrow a span of `others`, which the layer's own reading replaces.
    fn layer(&self, _plain: &str, floor: Vec<Hide>, others: Vec<Hide>) -> Vec<Hide> {
        union(vec![floor, others])
    }

    /// What a text that no reading settles is hidden as.
    fn whole(&self, _raw: &str) -> String {
        String::from(REDACTED)
    }

    /// The spans this layer hides over the stripped text when it reads the text by itself, and
    /// not as the union of the floor, the reads added since and its own `layer`. Only a reader
    /// that must replay what an older entry point answers (`Tail`) has one; the others read the
    /// text as [`hides`] does.
    fn own_spans(&self, _plain: &str, _marked: bool) -> Option<Vec<Hide>> {
        None
    }
}

/// No layer: text that is read as text.
pub(super) struct Plain;

impl Layer for Plain {}

/// A stderr tail: the floor and the well-known tokens, and none of the newer reads.
///
/// The tail is read once, when a failure is being reported, and it is a child's diagnostic
/// output, so the reads that cross lines, sequences and block scalars, which would hide more of
/// ordinary output than a diagnostic can spare, are left out. What it adds to the floor is what
/// has a shape of its own: a token with a known prefix, and a private key whose begin marker the
/// tail's cut dropped (`tokens::tail_hides`).
pub(super) struct Tail;

impl Layer for Tail {
    fn own_spans(&self, plain: &str, marked: bool) -> Option<Vec<Hide>> {
        Some(union(vec![
            floor::current_hides(plain, marked),
            tokens::tail_hides(plain),
        ]))
    }
}

/// A stderr tail whose first byte is inside a private key: [`Tail`], and the lines of the key that
/// the cut left at its start (`tokens::leading_key_run_hides`).
pub(super) struct TailInsideKey;

impl Layer for TailInsideKey {
    fn own_spans(&self, plain: &str, marked: bool) -> Option<Vec<Hide>> {
        Some(union(vec![
            floor::current_hides(plain, marked),
            tokens::tail_hides(plain),
            tokens::leading_key_run_hides(plain),
        ]))
    }
}

/// The spans of `plain`, the text the stripper left, that every reader and `layer` hide.
///
/// The readers are the rules of the release before (`floor`), the reads added since
/// (`read_hides`), and the well-known tokens. None of them sees what another has changed, so none
/// of them can make another miss its place. A layer with [`Layer::own_spans`] answers by itself.
pub(super) fn hides(plain: &str, marked: bool, layer: &dyn Layer) -> Vec<Hide> {
    if let Some(own) = layer.own_spans(plain, marked) {
        return own;
    }
    let floor = floor::hides(plain, marked);
    let others = union(vec![super::read_hides(plain), tokens::hides(plain)]);
    layer.layer(plain, floor, others)
}

/// `plain` with `hides` applied and the stripper's markers taken out.
pub(super) fn finish(plain: &str, hides: &[Hide]) -> String {
    remove_boundaries(apply(plain, hides))
}

/// `raw` read once by every reader and `layer`: the text with the union of the spans replaced.
pub(super) fn read_once(raw: &str, layer: &dyn Layer) -> String {
    let (plain, marked) = strip::strip_marking(raw);
    finish(&plain, &hides(&plain, marked, layer))
}

#[cfg(test)]
mod tests {
    use super::{Hide, apply, compose, union};
    use crate::redact::search::Random;

    #[test]
    fn spans_are_applied_in_order_and_the_text_between_them_is_kept() {
        let hides = [Hide::written(3, 8, "=", ""), Hide::taken_out(9, 10)];
        let received = apply("key: abcdXe", &hides);
        assert_eq!(
            received, "key=[REDACTED]de",
            "expected the value replaced and a byte taken out | received: {received:?}"
        );
        assert_eq!(apply("nothing", &[]), "nothing");
    }

    #[test]
    fn overlapping_spans_are_one_span_written_as_the_first_and_closed_as_the_last() {
        let first = vec![Hide::written(3, 8, "=", "")];
        let second = vec![Hide::marker(5, 12)];
        let received = union(vec![first.clone(), second.clone()]);
        assert_eq!(
            received,
            vec![Hide::written(3, 12, "=", "")],
            "expected the span to start as the first starts and end where the last ends"
        );
        // Spans that only touch are two.
        let touching = union(vec![vec![Hide::marker(0, 3)], vec![Hide::marker(3, 5)]]);
        assert_eq!(
            touching.len(),
            2,
            "expected touching spans kept apart | received: {touching:?}"
        );
        // A tail comes from the span that ends last, and the earlier list wins a tie.
        let tails = union(vec![
            vec![Hide::written(0, 5, "", "\"")],
            vec![Hide::written(2, 5, "", "'")],
        ]);
        assert_eq!(tails, vec![Hide::written(0, 5, "", "\"")]);
        // A byte taken out takes the marker of the span it meets.
        let met = union(vec![vec![Hide::taken_out(0, 2)], vec![Hide::marker(1, 4)]]);
        assert_eq!(
            met,
            vec![Hide {
                start: 0,
                end: 4,
                ..Hide::marker(0, 4)
            }]
        );
    }

    #[test]
    fn a_union_hides_everything_each_list_hides() {
        let mut random = Random::new(7, 0);
        for _ in 0..2_000 {
            let lists: Vec<Vec<Hide>> = (0..1 + random.below(3))
                .map(|_| random_hides(&mut random, 40))
                .collect();
            let merged = union(lists.clone());
            for hide in lists.iter().flatten() {
                let covered = merged
                    .iter()
                    .any(|span| span.start <= hide.start && hide.end <= span.end);
                assert!(
                    covered,
                    "expected {hide:?} inside a span of the union | received: {merged:?} of {lists:?}"
                );
            }
            assert!(
                merged.windows(2).all(|pair| pair[0].end <= pair[1].start),
                "expected the spans of a union in order and apart | received: {merged:?}"
            );
        }
    }

    /// Spans in order and apart over a text of `len` bytes.
    fn random_hides(random: &mut Random, len: usize) -> Vec<Hide> {
        let mut hides = Vec::new();
        let mut at = random.below(4);
        while at < len {
            let end = (at + 1 + random.below(6)).min(len);
            hides.push(match random.below(4) {
                0 => Hide::taken_out(at, end),
                1 => Hide::written(at, end, "=", ""),
                2 => Hide::written(at, end, "", "\""),
                _ => Hide::marker(at, end),
            });
            at = end + random.below(6);
        }
        hides
    }

    #[test]
    fn two_passes_in_a_row_are_one_list_over_the_text_the_first_read() {
        let mut random = Random::new(3, 0);
        for _ in 0..5_000 {
            let base: String = (0..40)
                .map(|_| char::from(b'a' + u8::try_from(random.below(26)).unwrap_or(0)))
                .collect();
            let first = random_hides(&mut random, base.len());
            let middle = apply(&base, &first);
            let second = random_hides(&mut random, middle.len());
            let expected = apply(&middle, &second);
            let both = compose(first.clone(), &middle, second.clone());
            let received = apply(&base, &both);
            assert_eq!(
                received, expected,
                "expected the composed list to give the two passes' answer | base {base:?} first {first:?} second {second:?} received {both:?}"
            );
        }
    }
}
