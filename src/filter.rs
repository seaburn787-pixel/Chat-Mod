//! Word-list matching with evasion normalization.
//!
//! Message and rule text both go through the same fold (accents stripped,
//! lookalike letters mapped, leetspeak decoded, spaced-out letters joined), so a
//! rule only needs its plain spelling. Each rule letter then matches one or more
//! repeats ("scaaam"), which avoids collapsing "ass" into the common word "as".

use crate::model::FlagHit;
use regex::{Regex, RegexBuilder};
use std::fs;
use std::path::Path;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

enum Matcher {
    /// `spaced` matches normal text; `joined` matches the same rule with the
    /// spaces between its words removed, for letters spelled across messages.
    Word {
        spaced: Regex,
        joined: Regex,
    },
    Pattern(Regex),
    User(String),
    /// `name:` rule: flags every message from anyone whose name contains this.
    Name(Regex),
}

struct Rule {
    source: String,
    matcher: Matcher,
    /// Contains-anywhere version of a word rule, for checking usernames.
    in_name: Option<Regex>,
}

pub struct WordList {
    pub name: String,
    pub enabled: bool,
    /// Also look for this list's words inside usernames.
    pub check_names: bool,
    rules: Vec<Rule>,
}

impl WordList {
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// Names in this file are never flagged for containing a list word.
pub const NAME_ALLOW_LIST: &str = "names_ok";

#[derive(Default)]
pub struct FilterSet {
    pub lists: Vec<WordList>,
    /// Folded names from names_ok.txt.
    name_allow: Vec<String>,
    pub errors: Vec<String>,
}

impl FilterSet {
    pub fn load(dir: &Path, disabled: &[String], name_lists: &[String]) -> Self {
        let mut set = FilterSet::default();
        let mut files: Vec<_> = match fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("txt")))
                .collect(),
            Err(e) => {
                set.errors
                    .push(format!("Can't read {}: {e}", dir.display()));
                return set;
            }
        };
        files.sort();
        for path in files {
            let name = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    set.errors.push(format!("{name}: {e}"));
                    continue;
                }
            };
            if name.eq_ignore_ascii_case(NAME_ALLOW_LIST) {
                set.name_allow = rule_lines(&text)
                    .map(|(_, l)| fold_name(l))
                    .filter(|l| !l.is_empty())
                    .collect();
                continue;
            }
            let mut rules = Vec::new();
            for (n, line) in rule_lines(&text) {
                match make_rule(line) {
                    Ok(rule) => rules.push(rule),
                    Err(e) => set.errors.push(format!("{name}.txt line {}: {e}", n + 1)),
                }
            }
            let enabled = !disabled.contains(&name);
            let check_names = name_lists.contains(&name);
            set.lists.push(WordList {
                name,
                enabled,
                check_names,
                rules,
            });
        }
        set
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        if let Some(l) = self.lists.iter_mut().find(|l| l.name == name) {
            l.enabled = enabled;
        }
    }

    pub fn active_rule_count(&self) -> usize {
        self.lists
            .iter()
            .filter(|l| l.enabled)
            .map(|l| l.rules.len())
            .sum()
    }

    pub fn check(&self, author: &str, channel_id: &str, text: &str) -> Vec<FlagHit> {
        let normalized = normalize(text);
        let author_key = author.trim().trim_start_matches('@').to_lowercase();
        let author_folded = fold_name(author);
        let channel_key = channel_id.to_lowercase();
        let mut hits = Vec::new();
        for list in self.lists.iter().filter(|l| l.enabled) {
            for rule in &list.rules {
                let matched = match &rule.matcher {
                    Matcher::Word { spaced, .. } => spaced.is_match(&normalized),
                    Matcher::Pattern(re) => re.is_match(text),
                    Matcher::User(u) => {
                        *u == author_key || (!channel_key.is_empty() && *u == channel_key)
                    }
                    Matcher::Name(re) => re.is_match(&author_folded),
                };
                if matched {
                    hits.push(FlagHit {
                        list: list.name.clone(),
                        rule: rule.source.clone(),
                    });
                }
            }
        }
        hits
    }

    /// List words found anywhere inside a username ("CoolXyzGuy" contains "xyz"),
    /// for lists with name checking on. Names in names_ok.txt are skipped.
    pub fn check_name(&self, author: &str) -> Vec<FlagHit> {
        let folded = fold_name(author);
        if folded.is_empty() || self.name_allow.contains(&folded) {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for list in self.lists.iter().filter(|l| l.enabled && l.check_names) {
            for rule in &list.rules {
                if rule.in_name.as_ref().is_some_and(|re| re.is_match(&folded)) {
                    hits.push(FlagHit {
                        list: list.name.clone(),
                        rule: format!("name contains: {}", rule.source),
                    });
                }
            }
        }
        hits
    }

    /// Checks text pieced together from several messages: `letters` is every
    /// fragment's folded letters run together, `raw` the fragments as typed with
    /// nothing between them. User rules already ran on each message.
    pub fn check_assembled(&self, letters: &str, raw: &str) -> Vec<FlagHit> {
        let normalized = normalize(letters);
        let mut hits = Vec::new();
        for list in self.lists.iter().filter(|l| l.enabled) {
            for rule in &list.rules {
                let matched = match &rule.matcher {
                    Matcher::Word { joined, .. } => joined.is_match(&normalized),
                    Matcher::Pattern(re) => re.is_match(raw),
                    Matcher::User(_) | Matcher::Name(_) => false,
                };
                if matched {
                    hits.push(FlagHit {
                        list: list.name.clone(),
                        rule: rule.source.clone(),
                    });
                }
            }
        }
        hits
    }

    #[cfg(test)]
    pub fn from_rules(list: &str, rules: &str) -> Self {
        let rules = rule_lines(rules)
            .map(|(_, l)| make_rule(l).unwrap())
            .collect();
        FilterSet {
            lists: vec![WordList {
                name: list.into(),
                enabled: true,
                check_names: true,
                rules,
            }],
            name_allow: vec![],
            errors: vec![],
        }
    }
}

/// Non-blank, non-comment lines with their 0-based line numbers.
fn rule_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .map(str::trim)
        .enumerate()
        .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
}

fn make_rule(line: &str) -> Result<Rule, String> {
    let matcher = parse_rule(line)?;
    let in_name = match matcher {
        Matcher::Word { .. } => contains_pattern(line).and_then(|p| Regex::new(&p).ok()),
        _ => None,
    };
    Ok(Rule {
        source: line.to_string(),
        matcher,
        in_name,
    })
}

/// A username folded for contains-matching: "@C00l_X.y.z_Guy" -> "cooixyzguy".
/// The leading @ of YouTube handles would otherwise fold to "a".
pub fn fold_name(name: &str) -> String {
    fold_letters(name.trim().trim_start_matches('@'))
}

/// All the letters in `text` after evasion folding, run together: "Y." -> "y".
pub fn fold_letters(text: &str) -> String {
    fold_tokens(text).concat()
}

/// True if `text` has real letters, ignoring emoji and :emote_shortcuts:.
pub fn has_letters(text: &str) -> bool {
    text.split_whitespace()
        .filter(|w| !(w.len() > 2 && w.starts_with(':') && w.ends_with(':')))
        .any(|w| !fold_letters(w).is_empty())
}

fn parse_rule(line: &str) -> Result<Matcher, String> {
    if let Some(re) = line.strip_prefix("re:") {
        return RegexBuilder::new(re.trim())
            .case_insensitive(true)
            .build()
            .map(Matcher::Pattern)
            .map_err(|e| format!("bad regex: {e}"));
    }
    if let Some(user) = line.strip_prefix("user:") {
        let user = user.trim().trim_start_matches('@').to_lowercase();
        if user.is_empty() {
            return Err("empty user rule".into());
        }
        return Ok(Matcher::User(user));
    }
    if let Some(name) = line.strip_prefix("name:") {
        let pattern = contains_pattern(name.trim()).ok_or("empty name rule")?;
        return Regex::new(&pattern)
            .map(Matcher::Name)
            .map_err(|e| e.to_string());
    }
    let spaced = word_pattern(line, false).ok_or("rule has no letters or digits")?;
    let joined = word_pattern(line, true).ok_or("rule has no letters or digits")?;
    Ok(Matcher::Word {
        spaced: Regex::new(&spaced).map_err(|e| e.to_string())?,
        joined: Regex::new(&joined).map_err(|e| e.to_string())?,
    })
}

/// Like the joined word pattern but without word boundaries, for matching
/// anywhere inside a folded username.
fn contains_pattern(rule: &str) -> Option<String> {
    word_pattern(rule, true).map(|p| p.trim_matches(' ').to_string())
}

/// Builds a regex over `normalize()` output. An edge `*` extends the word; an
/// inner `*` may also cross one separator, so "whats*app" hits "whats app".
/// `joined` drops the spaces between the rule's words ("free robux" ->
/// "freerobux"), since letters typed one per message never have spaces.
fn word_pattern(rule: &str, joined: bool) -> Option<String> {
    let pieces: Vec<Vec<String>> = rule.split('*').map(fold_tokens).collect();
    if pieces.iter().all(Vec::is_empty) {
        return None;
    }
    let mut pat = String::from(" ");
    for (i, tokens) in pieces.iter().enumerate() {
        if i > 0 {
            let inner = !pieces[i - 1].is_empty() && !tokens.is_empty();
            pat.push_str(if inner && !joined {
                r"\S*(?: \S*)?"
            } else {
                r"\S*"
            });
        }
        for (t, token) in tokens.iter().enumerate() {
            if t > 0 && !joined {
                pat.push(' ');
            }
            for c in token.chars() {
                pat.push_str(&regex::escape(&c.to_string()));
                pat.push('+');
            }
        }
    }
    pat.push(' ');
    Some(pat)
}

/// Space-padded, folded token string, e.g. "S.C.A.M 4 u!" -> " scam a u ".
pub fn normalize(text: &str) -> String {
    let tokens = fold_tokens(text);
    let mut out = String::with_capacity(text.len() + 2);
    out.push(' ');
    for t in tokens {
        out.push_str(&t);
        out.push(' ');
    }
    out
}

fn fold_tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .map(homoglyph)
        .collect();

    let mut tokens = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let mapped = match c {
            // "sh!t" is a letter, "wow!" is punctuation.
            '!' | '|' | '¡' => {
                let prev = i.checked_sub(1).and_then(|p| chars.get(p));
                let between_letters = prev.is_some_and(|&p| is_wordish(p))
                    && chars.get(i + 1).is_some_and(|&n| is_wordish(n));
                between_letters.then_some('i')
            }
            _ => leet(c).or_else(|| c.is_alphanumeric().then_some(c)),
        };
        match mapped {
            Some(m) => cur.push(m),
            None if !cur.is_empty() => tokens.push(std::mem::take(&mut cur)),
            None => {}
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    merge_single_letters(tokens)
}

fn is_wordish(c: char) -> bool {
    c.is_alphanumeric() || leet(c).is_some()
}

/// "s c a m" / "s.c.a.m" -> "scam". A lone single letter ("a", "i") is left alone.
fn merge_single_letters(tokens: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut run = String::new();
    let mut run_len = 0;
    let flush = |out: &mut Vec<String>, run: &mut String, run_len: &mut usize| {
        if *run_len > 0 {
            out.push(std::mem::take(run));
            *run_len = 0;
        }
    };
    for t in tokens {
        if t.chars().count() == 1 {
            run.push_str(&t);
            run_len += 1;
        } else {
            flush(&mut out, &mut run, &mut run_len);
            out.push(t);
        }
    }
    flush(&mut out, &mut run, &mut run_len);
    out
}

fn leet(c: char) -> Option<char> {
    Some(match c {
        '0' => 'o',
        // l and 1 fold to i on both sides, so "l1ar" == "liar" == "iiar".
        '1' | 'l' => 'i',
        '3' | '€' => 'e',
        '4' | '@' => 'a',
        '5' | '$' => 's',
        '7' | '+' => 't',
        '8' => 'b',
        '9' => 'g',
        _ => return None,
    })
}

/// Cyrillic/Greek letters that render like Latin ones. NFKD already handles
/// fullwidth, circled, and math-styled letters.
fn homoglyph(c: char) -> char {
    match c {
        'а' | 'α' => 'a',
        'в' | 'β' | 'ь' => 'b',
        'с' | 'ϲ' => 'c',
        'ԁ' => 'd',
        'е' | 'ё' | 'ε' | 'є' => 'e',
        'ɡ' => 'g',
        'һ' | 'н' => 'h',
        'і' | 'ї' | 'ι' | 'ı' => 'i',
        'ј' => 'j',
        'к' | 'κ' => 'k',
        'ӏ' => 'l',
        'м' => 'm',
        'п' | 'η' => 'n',
        'о' | 'ο' | 'σ' => 'o',
        'р' | 'ρ' => 'p',
        'ԛ' => 'q',
        'г' => 'r',
        'ѕ' => 's',
        'т' | 'τ' => 't',
        'υ' | 'μ' => 'u',
        'ν' => 'v',
        'ω' | 'ш' => 'w',
        'х' | 'χ' => 'x',
        'у' | 'γ' => 'y',
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(rules: &str) -> FilterSet {
        let mut lists = Vec::new();
        let rules = rules
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| make_rule(l).unwrap())
            .collect();
        lists.push(WordList {
            name: "t".into(),
            enabled: true,
            check_names: false,
            rules,
        });
        FilterSet {
            lists,
            name_allow: vec![],
            errors: vec![],
        }
    }

    fn flags(s: &FilterSet, text: &str) -> bool {
        !s.check("someone", "UCabc", text).is_empty()
    }

    #[test]
    fn plain_and_whole_word() {
        let s = set("scam");
        assert!(flags(&s, "this is a SCAM"));
        assert!(flags(&s, "scam."));
        assert!(!flags(&s, "scampi is tasty"));
    }

    #[test]
    fn short_word_does_not_hit_common_words() {
        let s = set("ass");
        assert!(flags(&s, "what an ass"));
        assert!(flags(&s, "@$$"));
        assert!(flags(&s, "a s s"));
        assert!(flags(&s, "aaasssss"));
        assert!(!flags(&s, "as I said"));
        assert!(!flags(&s, "class is in session"));
        assert!(!flags(&s, "pass the salt"));
    }

    #[test]
    fn evasion_forms() {
        let s = set("scam");
        for t in [
            "sc4m",
            "$c@m",
            "s c a m",
            "s.c.a.m",
            "s-c-a-m!!",
            "scaaaam",
            "ѕсаm",     // Cyrillic lookalikes
            "ｓｃａｍ", // fullwidth
            "𝐬𝐜𝐚𝐦",     // math bold
            "scàm",
        ] {
            assert!(flags(&s, t), "should flag {t:?}");
        }
    }

    #[test]
    fn bang_and_pipe_as_letters() {
        let s = set("shit");
        assert!(flags(&s, "sh!t"));
        assert!(flags(&s, "sh|t"));
        assert!(!flags(&s, "shh! it is fine"));
    }

    #[test]
    fn l_and_1_fold_together() {
        let s = set("liar");
        assert!(flags(&s, "l1ar"));
        assert!(flags(&s, "1iar"));
    }

    #[test]
    fn phrases_and_wildcards() {
        let s = set("free robux\nwhats*app\nhack*\n*coin");
        assert!(flags(&s, "get FREE R0BUX now"));
        assert!(!flags(&s, "free stuff, robux later"));
        assert!(flags(&s, "add me on whatsapp"));
        assert!(flags(&s, "whats app me"));
        assert!(flags(&s, "hackers everywhere"));
        assert!(flags(&s, "buy bitcoin"));
        assert!(!flags(&s, "coins are shiny"));
    }

    #[test]
    fn regex_and_user_rules() {
        let s = set("re:t\\.me/\nuser:BadGuy\nuser:UCxyz");
        assert!(flags(&s, "join t.me/whatever"));
        assert!(!s.check("badguy", "", "hello").is_empty());
        assert!(!s.check("@BadGuy", "", "hello").is_empty());
        assert!(!s.check("other", "UCxyz", "hello").is_empty());
        assert!(s.check("other", "UCq", "hello").is_empty());
    }

    #[test]
    fn disabled_list_is_skipped() {
        let mut s = set("scam");
        s.set_enabled("t", false);
        assert!(!flags(&s, "scam"));
    }

    #[test]
    fn rejects_empty_rules() {
        assert!(parse_rule("***").is_err());
        assert!(parse_rule("re:(").is_err());
    }

    #[test]
    fn list_words_inside_names() {
        let s = FilterSet::from_rules("profanity", "xyz");
        for name in [
            "CoolXyzGuy",
            "@xyz",
            "xX_xyz_Xx",
            "C00l_X.y.Z_guy",
            "xyyyyz123",
            "ｘｙｚ",
        ] {
            assert!(!s.check_name(name).is_empty(), "missed {name:?}");
        }
        for name in ["CoolGuy", "@xylophone", "x_y"] {
            assert!(s.check_name(name).is_empty(), "false hit {name:?}");
        }
        assert_eq!(s.check_name("bigxyzfan")[0].rule, "name contains: xyz");
    }

    #[test]
    fn name_checking_is_per_list() {
        let mut s = FilterSet::from_rules("spam", "xyz");
        s.lists[0].check_names = false;
        assert!(s.check_name("CoolXyzGuy").is_empty());
    }

    #[test]
    fn allow_list_exempts_names() {
        let mut s = FilterSet::from_rules("profanity", "ass");
        assert!(!s.check_name("Cassandra").is_empty());
        s.name_allow = vec![fold_name("@Cassandra")];
        assert!(s.check_name("Cassandra").is_empty());
        assert!(!s.check_name("BadAssDude").is_empty());
    }

    #[test]
    fn name_rule_flags_every_message() {
        let s = FilterSet::from_rules("watch", "name:johnny");
        for name in ["Johnny", "@xX_J0hnny_Xx", "johnnnnyV2", "big_john_ny"] {
            assert!(!s.check(name, "UC1", "hello").is_empty(), "missed {name:?}");
        }
        assert!(s.check("John", "UC1", "hello").is_empty());
        // Message text containing the name doesn't trigger it.
        assert!(s.check("Someone", "UC1", "johnny is here").is_empty());
    }
}
