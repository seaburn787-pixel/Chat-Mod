//! Catches words spelled out across several messages, one or two letters at a
//! time ("x" / "y" / "z"). Each author gets a short buffer of their recent
//! tiny messages; every new fragment re-checks the buffer's tail against the
//! word lists.

use crate::filter::{self, FilterSet};
use crate::model::FlagHit;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// A message counts as a fragment when it folds down to this many letters or fewer.
pub const MAX_FRAGMENT_LETTERS: usize = 3;
/// Fragments older than this no longer join up with new ones.
pub const SPELL_WINDOW: Duration = Duration::from_secs(60);
/// Longest phrase that can be assembled, in messages ("double your money" is 15).
const MAX_FRAGMENTS: usize = 24;
/// Stale buffers are swept once the map grows past this many authors.
const SWEEP_AT: usize = 300;

struct Fragment {
    id: String,
    raw: String,
    letters: String,
    at: Instant,
}

pub struct SpelledHit {
    pub hits: Vec<FlagHit>,
    /// Message ids that make up the word, oldest first (includes the newest one).
    pub ids: Vec<String>,
    /// The fragments as typed, joined for display: "x · y · z".
    pub combined: String,
}

#[derive(Default)]
pub struct SpellTracker {
    buffers: HashMap<(u64, String), VecDeque<Fragment>>,
}

impl SpellTracker {
    /// Feeds one message from `author` (keyed per stream). Returns a hit when this
    /// message completes a filtered word with the author's previous fragments.
    pub fn push(
        &mut self,
        filters: &FilterSet,
        key: (u64, String),
        id: &str,
        text: &str,
        now: Instant,
    ) -> Option<SpelledHit> {
        if self.buffers.len() > SWEEP_AT {
            self.buffers.retain(|_, b| {
                b.back()
                    .is_some_and(|f| now.duration_since(f.at) < SPELL_WINDOW)
            });
        }
        let Some(letters) = fragment_letters(text) else {
            // A real sentence breaks the chain; emote/emoji-only messages don't.
            if filter::has_letters(text) {
                self.buffers.remove(&key);
            }
            return None;
        };

        let buf = self.buffers.entry(key.clone()).or_default();
        while buf
            .front()
            .is_some_and(|f| now.duration_since(f.at) >= SPELL_WINDOW)
        {
            buf.pop_front();
        }
        buf.push_back(Fragment {
            id: id.to_string(),
            raw: text.trim().to_string(),
            letters,
            at: now,
        });
        while buf.len() > MAX_FRAGMENTS {
            buf.pop_front();
        }

        // Every suffix of 2+ fragments ending at this one: leading chatter
        // ("a", "x", "y", "z") must not hide the word.
        let n = buf.len();
        for start in 0..n.saturating_sub(1) {
            let joined: String = buf.iter().skip(start).map(|f| f.letters.as_str()).collect();
            let raw: String = buf.iter().skip(start).map(|f| f.raw.as_str()).collect();
            let hits = filters.check_assembled(&joined, &raw);
            if !hits.is_empty() {
                let parts: Vec<&Fragment> = buf.iter().skip(start).collect();
                let result = SpelledHit {
                    hits: hits
                        .into_iter()
                        .map(|h| FlagHit {
                            list: h.list,
                            rule: format!("{} (spelled over {} messages)", h.rule, parts.len()),
                        })
                        .collect(),
                    ids: parts.iter().map(|f| f.id.clone()).collect(),
                    combined: parts
                        .iter()
                        .map(|f| f.raw.as_str())
                        .collect::<Vec<_>>()
                        .join(" · "),
                };
                // Start fresh so the next letter doesn't re-flag the same word.
                self.buffers.remove(&key);
                return Some(result);
            }
        }
        None
    }
}

/// Folded letters if `text` is a tiny fragment, else None. Bare punctuation
/// ("." or "/") counts too, with no letters, so links typed in pieces join up.
fn fragment_letters(text: &str) -> Option<String> {
    let t = text.trim();
    // Raw length cap keeps links and long emote names out even if they fold short.
    if t.is_empty() || t.chars().count() > MAX_FRAGMENT_LETTERS * 2 + 2 {
        return None;
    }
    if !filter::has_letters(t) {
        let punct = t.chars().all(|c| c.is_ascii_punctuation());
        return punct.then(String::new);
    }
    let letters = filter::fold_letters(t);
    // Leet folding turns digits into letters ("555" -> "sss"), so count what was
    // typed: a 3-digit chunk of a phone number is still a fragment.
    let count = letters
        .chars()
        .count()
        .min(t.chars().filter(|c| c.is_alphanumeric()).count());
    (1..=MAX_FRAGMENT_LETTERS)
        .contains(&count)
        .then_some(letters)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters(rules: &str) -> FilterSet {
        FilterSet::from_rules("slurs", rules)
    }

    fn feed(
        t: &mut SpellTracker,
        f: &FilterSet,
        who: &str,
        msgs: &[&str],
        start: Instant,
        gap: Duration,
    ) -> Vec<Option<SpelledHit>> {
        msgs.iter()
            .enumerate()
            .map(|(i, m)| {
                t.push(
                    f,
                    (0, who.to_string()),
                    &format!("{who}-{i}"),
                    m,
                    start + gap * i as u32,
                )
            })
            .collect()
    }

    const GAP: Duration = Duration::from_secs(2);

    #[test]
    fn catches_one_letter_per_message() {
        let f = filters("xyz");
        let mut t = SpellTracker::default();
        let r = feed(&mut t, &f, "bob", &["x", "y", "z"], Instant::now(), GAP);
        assert!(r[0].is_none() && r[1].is_none());
        let hit = r[2].as_ref().expect("should flag on the last letter");
        assert_eq!(hit.ids, ["bob-0", "bob-1", "bob-2"]);
        assert_eq!(hit.combined, "x · y · z");
        assert!(hit.hits[0].rule.contains("spelled over 3 messages"));
    }

    #[test]
    fn evasion_tricks_inside_fragments() {
        let f = filters("xyz");
        for msgs in [
            &["X", "y.", "Z!"][..],
            &["x", "yz"][..],
            &["xy", "z"][..],
            &["x", "y", "y", "z"][..], // stretched
            &["ｘ", "у", "z"][..],     // fullwidth x, Cyrillic у
        ] {
            let mut t = SpellTracker::default();
            let r = feed(&mut t, &f, "bob", msgs, Instant::now(), GAP);
            assert!(r.last().unwrap().is_some(), "missed {msgs:?}");
        }
    }

    #[test]
    fn leading_letters_and_emotes_dont_hide_it() {
        let f = filters("xyz");
        let mut t = SpellTracker::default();
        let r = feed(
            &mut t,
            &f,
            "bob",
            &["a", "x", "😀", "y", "z"],
            Instant::now(),
            GAP,
        );
        let hit = r[4].as_ref().expect("should flag");
        assert_eq!(hit.ids, ["bob-1", "bob-3", "bob-4"]);
    }

    #[test]
    fn other_people_dont_mix_in() {
        let f = filters("xyz");
        let mut t = SpellTracker::default();
        let now = Instant::now();
        assert!(t.push(&f, (0, "a".into()), "1", "x", now).is_none());
        assert!(t.push(&f, (0, "b".into()), "2", "y", now).is_none());
        assert!(t.push(&f, (0, "a".into()), "3", "z", now).is_none());
        // Same name on a different stream is a different person.
        assert!(t.push(&f, (1, "a".into()), "4", "y", now).is_none());
    }

    #[test]
    fn sentence_breaks_the_chain() {
        let f = filters("xyz");
        let mut t = SpellTracker::default();
        let r = feed(
            &mut t,
            &f,
            "bob",
            &["x", "y", "hello there", "z"],
            Instant::now(),
            GAP,
        );
        assert!(r.iter().all(Option::is_none));
    }

    #[test]
    fn too_slow_doesnt_join() {
        let f = filters("xyz");
        let mut t = SpellTracker::default();
        let r = feed(
            &mut t,
            &f,
            "bob",
            &["x", "y", "z"],
            Instant::now(),
            SPELL_WINDOW / 2 + GAP,
        );
        assert!(r[2].is_none());
    }

    #[test]
    fn whole_word_rule_still_whole_word() {
        // "ass" spelled out is caught; "a","s" + "k" ("ask") is not.
        let f = filters("ass");
        let mut t = SpellTracker::default();
        assert!(feed(&mut t, &f, "bob", &["a", "s", "s"], Instant::now(), GAP)[2].is_some());
        let mut t = SpellTracker::default();
        let r = feed(&mut t, &f, "bob", &["a", "s", "k"], Instant::now(), GAP);
        assert!(r.iter().all(Option::is_none));
    }

    #[test]
    fn flags_once_then_starts_over() {
        let f = filters("xyz");
        let mut t = SpellTracker::default();
        let r = feed(
            &mut t,
            &f,
            "bob",
            &["x", "y", "z", "z"],
            Instant::now(),
            GAP,
        );
        assert!(r[2].is_some());
        assert!(r[3].is_none());
    }

    fn one_per_line(t: &mut SpellTracker, f: &FilterSet, word: &str) -> Option<SpelledHit> {
        let pieces: Vec<String> = word
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(String::from)
            .collect();
        let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
        feed(t, f, "bob", &refs, Instant::now(), GAP)
            .pop()
            .flatten()
    }

    #[test]
    fn multi_word_and_wildcard_rules_spelled_out() {
        let f = filters("free robux\ndouble your money\nwhats*app\ntelegram");
        for word in ["free robux", "double your money", "whatsapp", "telegram"] {
            let mut t = SpellTracker::default();
            assert!(one_per_line(&mut t, &f, word).is_some(), "missed {word:?}");
        }
        // Chunks of 2-3 letters too.
        let mut t = SpellTracker::default();
        let r = feed(
            &mut t,
            &f,
            "bob",
            &["fre", "ero", "bux"],
            Instant::now(),
            GAP,
        );
        assert!(r[2].is_some());
    }

    #[test]
    fn regex_rules_spelled_out() {
        let f = filters("re:(t\\.me|bit\\.ly)/\nre:\\+\\d[\\d\\s-]{9,}");
        let mut t = SpellTracker::default();
        let r = feed(&mut t, &f, "bob", &["t.", "me", "/"], Instant::now(), GAP);
        assert!(r[2].is_some(), "link pieces");
        let mut t = SpellTracker::default();
        let r = feed(
            &mut t,
            &f,
            "bob",
            &["+1", "555", "123", "456", "7"],
            Instant::now(),
            GAP,
        );
        assert!(r.iter().any(Option::is_some), "phone number pieces");
    }

    #[test]
    fn normal_short_replies_are_fine() {
        let f = filters("xyz");
        let mut t = SpellTracker::default();
        let r = feed(
            &mut t,
            &f,
            "bob",
            &["lol", "ok", "gg", "w", "hi"],
            Instant::now(),
            GAP,
        );
        assert!(r.iter().all(Option::is_none));
    }
}
