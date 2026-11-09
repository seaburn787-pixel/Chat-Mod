//! Spam that isn't on a word list: one person repeating the same message, or
//! many people pasting the same message (copy-paste raids). Messages are
//! compared after the filter's evasion folding, so "am i banned yet",
//! "AM I BANNED YET??" and "am i baned yet" count as the same message.

use crate::filter;
use crate::model::FlagHit;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Messages shorter than this many letters ("lol", "gg", "W") are ignored:
/// people repeat those naturally.
pub const MIN_LETTERS: usize = 5;
/// Fingerprints are cut to this many characters so a pasted essay can't use
/// much memory.
const MAX_FINGERPRINT: usize = 160;
const SWEEP_EVERY: Duration = Duration::from_secs(10);
pub const LIST_NAME: &str = "spam detection";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rule {
    /// How many times / how many people.
    pub count: usize,
    pub window: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Settings {
    /// Same person, same message.
    pub repeats: Option<Rule>,
    /// Different people, same message.
    pub raids: Option<Rule>,
}

pub struct RepeatHit {
    pub hit: FlagHit,
    /// Every message that counted toward the hit, oldest first (includes the new one).
    pub ids: Vec<String>,
}

struct Seen {
    fingerprint: String,
    id: String,
    at: Instant,
}

struct Post {
    author: String,
    id: String,
    at: Instant,
}

pub struct RepeatTracker {
    /// (stream, author) -> their recent messages.
    by_author: HashMap<(u64, String), VecDeque<Seen>>,
    /// (stream, fingerprint) -> who posted it recently.
    by_text: HashMap<(u64, String), VecDeque<Post>>,
    last_sweep: Instant,
}

impl Default for RepeatTracker {
    fn default() -> Self {
        Self {
            by_author: HashMap::new(),
            by_text: HashMap::new(),
            last_sweep: Instant::now(),
        }
    }
}

impl RepeatTracker {
    /// Feeds one message. `text` is the message without emotes.
    pub fn push(
        &mut self,
        settings: &Settings,
        source: u64,
        author: &str,
        id: &str,
        text: &str,
        now: Instant,
    ) -> Vec<RepeatHit> {
        self.sweep(settings, now);
        let Some(fp) = fingerprint(text) else {
            return Vec::new();
        };
        let mut hits = Vec::new();

        if let Some(rule) = settings.repeats {
            let seen = self
                .by_author
                .entry((source, author.to_string()))
                .or_default();
            seen.retain(|s| now.duration_since(s.at) <= rule.window);
            seen.push_back(Seen {
                fingerprint: fp.clone(),
                id: id.to_string(),
                at: now,
            });
            let same: Vec<&Seen> = seen.iter().filter(|s| s.fingerprint == fp).collect();
            if same.len() >= rule.count {
                let span = now.duration_since(same[0].at);
                hits.push(RepeatHit {
                    hit: FlagHit {
                        list: LIST_NAME.into(),
                        rule: format!("repeated {}× in {}", same.len(), secs(span)),
                    },
                    ids: same.iter().map(|s| s.id.clone()).collect(),
                });
                // Start counting again, so continued spam flags again later
                // instead of on every single copy.
                seen.retain(|s| s.fingerprint != fp);
            }
            // Nobody needs more history than the trigger count per message.
            while seen.len() > rule.count * 4 {
                seen.pop_front();
            }
        }

        if let Some(rule) = settings.raids {
            let posts = self.by_text.entry((source, fp)).or_default();
            posts.retain(|p| now.duration_since(p.at) <= rule.window);
            posts.push_back(Post {
                author: author.to_string(),
                id: id.to_string(),
                at: now,
            });
            let people: HashSet<&str> = posts.iter().map(|p| p.author.as_str()).collect();
            if people.len() >= rule.count {
                let span = now.duration_since(posts[0].at);
                hits.push(RepeatHit {
                    hit: FlagHit {
                        list: LIST_NAME.into(),
                        rule: format!(
                            "same message from {} people in {}",
                            people.len(),
                            secs(span)
                        ),
                    },
                    ids: posts.iter().map(|p| p.id.clone()).collect(),
                });
                posts.clear();
            }
        }
        hits
    }

    /// Drops everything older than the longest window, so memory only holds
    /// the last minute or so of chat.
    fn sweep(&mut self, settings: &Settings, now: Instant) {
        if now.duration_since(self.last_sweep) < SWEEP_EVERY {
            return;
        }
        self.last_sweep = now;
        let keep = |rule: Option<Rule>| rule.map_or(Duration::ZERO, |r| r.window);
        let author_window = keep(settings.repeats);
        self.by_author.retain(|_, seen| {
            seen.retain(|s| now.duration_since(s.at) <= author_window);
            !seen.is_empty()
        });
        let text_window = keep(settings.raids);
        self.by_text.retain(|_, posts| {
            posts.retain(|p| now.duration_since(p.at) <= text_window);
            !posts.is_empty()
        });
    }

    #[cfg(test)]
    fn tracked(&self) -> (usize, usize) {
        (self.by_author.len(), self.by_text.len())
    }
}

/// The message reduced to what a person would call "the same message":
/// evasion-folded words with stretched letters collapsed.
fn fingerprint(text: &str) -> Option<String> {
    let folded = filter::normalize(text);
    let mut out = String::new();
    let mut letters = 0;
    for c in folded.trim().chars() {
        if out.ends_with(c) {
            continue; // "baaanned" == "banned" == "baned"
        }
        if c.is_alphanumeric() {
            letters += 1;
        }
        out.push(c);
        if out.len() >= MAX_FINGERPRINT {
            break;
        }
    }
    (letters >= MIN_LETTERS).then_some(out)
}

fn secs(d: Duration) -> String {
    format!("{}s", d.as_secs().max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPEATS: Rule = Rule {
        count: 3,
        window: Duration::from_secs(60),
    };
    const RAIDS: Rule = Rule {
        count: 4,
        window: Duration::from_secs(30),
    };

    /// Shared clock origin so `at` offsets line up across calls.
    static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

    fn both() -> Settings {
        Settings {
            repeats: Some(REPEATS),
            raids: Some(RAIDS),
        }
    }

    /// Feeds a message `at` seconds after START; returns the hit descriptions.
    fn feed(t: &mut RepeatTracker, s: &Settings, at: u64, author: &str, text: &str) -> Vec<String> {
        let base = *START;
        t.push(
            s,
            0,
            author,
            &format!("{author}-{at}"),
            text,
            base + Duration::from_secs(at),
        )
        .into_iter()
        .map(|h| h.hit.rule)
        .collect()
    }

    #[test]
    fn same_person_same_message() {
        let (mut t, s) = (RepeatTracker::default(), both());
        assert!(feed(&mut t, &s, 0, "a", "am i banned yet").is_empty());
        assert!(feed(&mut t, &s, 10, "a", "AM I BANNED YET??").is_empty());
        assert_eq!(
            feed(&mut t, &s, 40, "a", "am i baaaned yet"),
            ["repeated 3× in 40s"]
        );
        // Counting starts over after a flag.
        assert!(feed(&mut t, &s, 41, "a", "am i banned yet").is_empty());
    }

    #[test]
    fn hit_lists_every_copy() {
        let (mut t, s) = (RepeatTracker::default(), both());
        let now = *START;
        for (i, at) in [0u64, 5, 9].into_iter().enumerate() {
            let hits = t.push(
                &s,
                0,
                "a",
                &format!("m{i}"),
                "buy my course now",
                now + Duration::from_secs(at),
            );
            if i == 2 {
                assert_eq!(hits[0].ids, ["m0", "m1", "m2"]);
            }
        }
    }

    #[test]
    fn spread_out_or_mixed_messages_dont_count() {
        let (mut t, s) = (RepeatTracker::default(), both());
        feed(&mut t, &s, 0, "a", "am i banned yet");
        feed(&mut t, &s, 30, "a", "am i banned yet");
        // First copy is now older than the 60s window.
        assert!(feed(&mut t, &s, 70, "a", "am i banned yet").is_empty());
        let (mut t, s) = (RepeatTracker::default(), both());
        feed(&mut t, &s, 0, "a", "am i banned yet");
        feed(&mut t, &s, 1, "a", "something else entirely");
        assert!(feed(&mut t, &s, 2, "a", "different again here").is_empty());
    }

    #[test]
    fn short_and_emote_only_messages_are_ignored() {
        let (mut t, s) = (RepeatTracker::default(), both());
        for i in 0..10 {
            assert!(feed(&mut t, &s, i, "a", "lol").is_empty());
            assert!(feed(&mut t, &s, i, &format!("u{i}"), "W").is_empty());
            assert!(feed(&mut t, &s, i, "b", "   ").is_empty());
        }
        assert_eq!(t.tracked(), (0, 0), "ignored messages aren't stored");
    }

    #[test]
    fn people_are_tracked_separately() {
        let (mut t, s) = (
            RepeatTracker::default(),
            Settings {
                repeats: Some(REPEATS),
                raids: None,
            },
        );
        for (i, who) in ["a", "b", "a", "b"].into_iter().enumerate() {
            assert!(feed(&mut t, &s, i as u64, who, "first time here hello").is_empty());
        }
    }

    #[test]
    fn copy_paste_raid() {
        let (mut t, s) = (RepeatTracker::default(), both());
        assert!(feed(&mut t, &s, 0, "bot1", "FREE GIFT CARDS at scam dot com").is_empty());
        assert!(feed(&mut t, &s, 2, "bot2", "free gift cards at scam dot com").is_empty());
        // The same person twice still counts as one person.
        assert!(feed(&mut t, &s, 3, "bot2", "free gift cards at scam dot com").is_empty());
        assert!(feed(&mut t, &s, 4, "bot3", "free gift cards at scam dot com").is_empty());
        assert_eq!(
            feed(&mut t, &s, 9, "bot4", "Free gift cards at scam dot com!"),
            ["same message from 4 people in 9s"]
        );
    }

    #[test]
    fn toggles_turn_detection_off() {
        let (mut t, s) = (RepeatTracker::default(), Settings::default());
        for i in 0..10 {
            assert!(
                feed(
                    &mut t,
                    &s,
                    i,
                    &format!("u{}", i % 2),
                    "same message every time"
                )
                .is_empty()
            );
        }
        assert_eq!(t.tracked(), (0, 0));
    }

    #[test]
    fn memory_is_swept() {
        let (mut t, s) = (RepeatTracker::default(), both());
        for i in 0..1000 {
            feed(
                &mut t,
                &s,
                0,
                &format!("user{i}"),
                &format!("unique message number {i}"),
            );
        }
        // Fewer texts than authors: "number 1" and "number 11" fold alike.
        let (authors, texts) = t.tracked();
        assert_eq!(authors, 1000);
        assert!(texts > 500);
        // A minute later, everything above has aged out.
        feed(&mut t, &s, 200, "late", "one more message here");
        assert_eq!(t.tracked(), (1, 1));
    }

    #[test]
    fn huge_messages_are_capped() {
        let long = "spam ".repeat(5000);
        assert!(fingerprint(&long).unwrap().len() <= MAX_FINGERPRINT + 4);
    }
}
