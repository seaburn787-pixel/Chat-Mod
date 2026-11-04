//! Twitch chat via anonymous read-only IRC. No account or key needed.

use super::*;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::TcpStream;
use std::sync::{OnceLock, RwLock};

/// Third-party (7TV / BTTV / FFZ) emote name -> image URL.
type EmoteMap = HashMap<String, String>;
type SharedEmotes = Arc<RwLock<Arc<EmoteMap>>>;

const HOST: &str = "irc.chat.twitch.tv:6667";
const MAX_FAILURES: u32 = 5;
/// Twitch drops idle clients after ~5 min without a PONG; it sends PING first.
const READ_TICK: Duration = Duration::from_millis(250);

/// "twitch.tv/name", "https://www.twitch.tv/name/...", or "twitch:name" -> "name".
pub fn parse_channel(target: &str) -> Option<String> {
    let t = target.trim();
    let rest = if let Some(pos) = t.to_ascii_lowercase().find("twitch.tv/") {
        &t[pos + "twitch.tv/".len()..]
    } else {
        t.strip_prefix("twitch:")
            .or_else(|| t.strip_prefix("tw:"))?
    };
    let name: String = rest
        .trim_start_matches('#')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

pub fn run(channel: &str, out: &Out) -> Result<(), String> {
    out.send(SourceEvent::Resolved {
        video_id: format!("#{channel}"),
        title: format!("{channel} (Twitch)"),
    });
    // Kept across reconnects so the emote sets are only fetched once.
    let emotes: SharedEmotes = Arc::default();
    let emotes_requested = Arc::new(AtomicBool::new(false));
    let mut failures = 0;
    loop {
        if out.stopped() {
            return Ok(());
        }
        out.status(format!("Connecting to Twitch #{channel}…"));
        match session(channel, &emotes, &emotes_requested, out) {
            Ok(()) => return Ok(()),
            Err(SessionError::Fatal(e)) => return Err(e),
            Err(SessionError::Dropped(e)) => {
                failures += 1;
                if failures >= MAX_FAILURES {
                    return Err(e);
                }
                out.status(format!("{e} — reconnecting ({failures}/{MAX_FAILURES})"));
                if !out.sleep(Duration::from_secs(2 * failures as u64)) {
                    return Ok(());
                }
            }
        }
    }
}

enum SessionError {
    /// Worth reconnecting (network blip, server restart).
    Dropped(String),
    /// Won't fix itself (channel banned/suspended).
    Fatal(String),
}

fn session(
    channel: &str,
    emotes: &SharedEmotes,
    emotes_requested: &Arc<AtomicBool>,
    out: &Out,
) -> Result<(), SessionError> {
    let drop = |e: std::io::Error| SessionError::Dropped(format!("Twitch connection: {e}"));
    let mut sock = TcpStream::connect(HOST).map_err(drop)?;
    sock.set_read_timeout(Some(READ_TICK)).map_err(drop)?;
    // justinfanNNNN is Twitch's documented anonymous read-only login.
    let nick = format!("justinfan{}", 10_000 + std::process::id() % 80_000);
    write!(
        sock,
        "CAP REQ :twitch.tv/tags twitch.tv/commands\r\nPASS SCHMOOPIIE\r\nNICK {nick}\r\nJOIN #{channel}\r\n"
    )
    .map_err(drop)?;

    let mut writer = sock.try_clone().map_err(drop)?;
    let mut reader = BufReader::new(sock);
    let mut line = String::new();
    loop {
        if out.stopped() {
            let _ = writer.write_all(b"QUIT\r\n");
            return Ok(());
        }
        match reader.read_line(&mut line) {
            Ok(0) => return Err(SessionError::Dropped("Twitch closed the connection".into())),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue; // partial line stays buffered in `line`
            }
            Err(e) => return Err(drop(e)),
        }
        let raw = std::mem::take(&mut line);
        let Some(msg) = Irc::parse(raw.trim_end_matches(['\r', '\n'])) else {
            continue;
        };
        match msg.command {
            "PING" => {
                writer
                    .write_all(
                        format!("PONG :{}\r\n", msg.trailing.unwrap_or("tmi.twitch.tv")).as_bytes(),
                    )
                    .map_err(drop)?;
            }
            "RECONNECT" => {
                return Err(SessionError::Dropped("Twitch asked us to reconnect".into()));
            }
            // ROOMSTATE arrives once the JOIN succeeded.
            // ROOMSTATE repeats on mode changes; only the first one triggers the fetch.
            "ROOMSTATE" => {
                out.status(format!("Connected (Twitch) · #{channel}"));
                if let Some(room_id) = msg.tag("room-id")
                    && !emotes_requested.swap(true, Ordering::Relaxed)
                {
                    let room_id = room_id.to_string();
                    let emotes = emotes.clone();
                    std::thread::spawn(move || {
                        let map = load_emotes(&room_id);
                        *emotes.write().unwrap() = Arc::new(map);
                    });
                }
            }
            "NOTICE" => {
                let text = msg.trailing.unwrap_or_default().to_string();
                match msg.tag("msg-id") {
                    Some("msg_channel_suspended" | "msg_banned") => {
                        return Err(SessionError::Fatal(format!("#{channel}: {text}")));
                    }
                    _ => out.status(format!("Twitch: {text}")),
                }
            }
            "PRIVMSG" => {
                let map = emotes.read().unwrap().clone();
                if let Some(m) = chat_message(&msg, None, &map) {
                    out.send(SourceEvent::Message(m));
                }
            }
            // Sub/resub/raid notices; only the ones with a viewer-written message matter here.
            "USERNOTICE" if msg.trailing.is_some_and(|t| !t.is_empty()) => {
                let kind = msg.tag("msg-id").unwrap_or("notice").to_string();
                let map = emotes.read().unwrap().clone();
                if let Some(m) = chat_message(&msg, Some(kind), &map) {
                    out.send(SourceEvent::Message(m));
                }
            }
            "CLEARMSG" => {
                if let Some(id) = msg.tag("target-msg-id") {
                    out.send(SourceEvent::Deleted(id.to_string()));
                }
            }
            // With a target user: ban/timeout. Without: whole chat cleared (ignored).
            "CLEARCHAT" => {
                if let Some(uid) = msg.tag("target-user-id") {
                    out.send(SourceEvent::DeletedByAuthor(uid.to_string()));
                }
            }
            _ => {}
        }
    }
}

fn chat_message(msg: &Irc, notice_kind: Option<String>, emotes: &EmoteMap) -> Option<ChatMsg> {
    let mut body = msg.trailing.unwrap_or_default();
    // "/me" actions arrive wrapped in CTCP ACTION.
    if let Some(inner) = body.strip_prefix("\u{1}ACTION ") {
        body = inner.trim_end_matches('\u{1}');
    }
    let login = msg
        .tag("login")
        .or_else(|| msg.prefix.and_then(|p| p.split('!').next()))
        .unwrap_or_default();
    let author = msg
        .tag("display-name")
        .filter(|d| !d.is_empty())
        .unwrap_or(login)
        .to_string();
    let badges = msg.tag("badges").unwrap_or_default();
    let has_badge = |b: &str| badges.split(',').any(|x| x.split('/').next() == Some(b));
    let superchat = msg.tag("bits").map(|b| format!("{b} bits")).or(notice_kind);

    Some(ChatMsg {
        source: 0,
        id: msg.tag("id")?.to_string(),
        author,
        channel_id: msg.tag("user-id").unwrap_or_default().to_string(),
        profile_url: (!login.is_empty()).then(|| format!("https://www.twitch.tv/{login}")),
        text: body.to_string(),
        parts: word_emotes(
            emote_parts(body, msg.tag("emotes").unwrap_or_default()),
            emotes,
        ),
        time: msg
            .tag("tmi-sent-ts")
            .and_then(|t| t.parse::<i64>().ok())
            .map(|ms| model::local_time_from_micros(ms * 1000))
            .unwrap_or_else(model::now_time),
        is_owner: has_badge("broadcaster"),
        is_mod: has_badge("moderator") || msg.tag("mod") == Some("1"),
        is_member: has_badge("subscriber") || has_badge("founder"),
        is_verified: has_badge("partner"),
        superchat,
        deleted: false,
        hits: Vec::new(),
        name_flagged: false,
        room_id: msg.tag("room-id").map(str::to_string),
        mod_status: Default::default(),
        yt_menu: None,
    })
}

/// `emotes` tag: "id:start-end,start-end/id2:start-end", indices in characters.
fn emote_parts(body: &str, emotes: &str) -> Vec<Part> {
    let mut ranges: Vec<(usize, usize, &str)> = Vec::new();
    for spec in emotes.split('/').filter(|s| !s.is_empty()) {
        let Some((id, spans)) = spec.split_once(':') else {
            continue;
        };
        for span in spans.split(',') {
            if let Some((a, b)) = span.split_once('-')
                && let (Ok(a), Ok(b)) = (a.parse(), b.parse())
            {
                ranges.push((a, b, id));
            }
        }
    }
    if ranges.is_empty() {
        return vec![Part::Text(body.to_string())];
    }
    ranges.sort_unstable();
    let chars: Vec<char> = body.chars().collect();
    let mut parts = Vec::new();
    let mut pos = 0;
    for (a, b, id) in ranges {
        if a < pos || b >= chars.len() {
            continue; // malformed or overlapping
        }
        if a > pos {
            parts.push(Part::Text(chars[pos..a].iter().collect()));
        }
        parts.push(Part::Emote {
            url: format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/1.0"),
            alt: chars[a..=b].iter().collect(),
        });
        pos = b + 1;
    }
    if pos < chars.len() {
        parts.push(Part::Text(chars[pos..].iter().collect()));
    }
    parts
}

/// Replaces whole words that are 7TV/BTTV/FFZ emote names. Twitch's own emotes
/// are already split out by position.
fn word_emotes(parts: Vec<Part>, emotes: &EmoteMap) -> Vec<Part> {
    if emotes.is_empty() {
        return parts;
    }
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        let Part::Text(text) = part else {
            out.push(part);
            continue;
        };
        let mut buf = String::new();
        for piece in text.split_inclusive(' ') {
            let word = piece.trim_end_matches(' ');
            match emotes.get(word) {
                Some(url) if !word.is_empty() => {
                    if !buf.is_empty() {
                        out.push(Part::Text(std::mem::take(&mut buf)));
                    }
                    out.push(Part::Emote {
                        url: url.clone(),
                        alt: word.to_string(),
                    });
                    buf.push_str(&piece[word.len()..]);
                }
                _ => buf.push_str(piece),
            }
        }
        if !buf.is_empty() {
            out.push(Part::Text(buf));
        }
    }
    out
}

/// Global + channel sets. Later inserts win, matching how chat extensions
/// layer them: FFZ < BTTV < 7TV, global < channel.
fn load_emotes(room_id: &str) -> EmoteMap {
    let http = Http::new();
    let mut map = global_emotes(&http).clone();
    let room = fetch(
        &http,
        &format!("https://api.frankerfacez.com/v1/room/id/{room_id}"),
    );
    ffz_into(&mut map, &room, None);
    let bttv = fetch(
        &http,
        &format!("https://api.betterttv.net/3/cached/users/twitch/{room_id}"),
    );
    bttv_into(&mut map, &bttv);
    let stv = fetch(&http, &format!("https://7tv.io/v3/users/twitch/{room_id}"));
    seventv_into(&mut map, &stv["emote_set"]);
    map
}

fn global_emotes(http: &Http) -> &'static EmoteMap {
    static GLOBAL: OnceLock<EmoteMap> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        let mut map = EmoteMap::new();
        let ffz = fetch(http, "https://api.frankerfacez.com/v1/set/global");
        let defaults = ffz["default_sets"].as_array().cloned().unwrap_or_default();
        ffz_into(&mut map, &ffz, Some(&defaults));
        bttv_into(
            &mut map,
            &fetch(http, "https://api.betterttv.net/3/cached/emotes/global"),
        );
        seventv_into(
            &mut map,
            &fetch(http, "https://7tv.io/v3/emote-sets/global"),
        );
        map
    })
}

/// Emote sets are optional decoration, so any failure just means fewer emotes.
fn fetch(http: &Http, url: &str) -> Value {
    match http.get_json(url) {
        Ok((status, v)) if status < 400 => v,
        _ => Value::Null,
    }
}

fn https(url: &str) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_string()
    }
}

fn ffz_into(map: &mut EmoteMap, v: &Value, only_sets: Option<&[Value]>) {
    let Some(sets) = v["sets"].as_object() else {
        return;
    };
    for (id, set) in sets {
        if let Some(only) = only_sets
            && !only
                .iter()
                .any(|x| x.as_u64().is_some_and(|n| n.to_string() == *id))
        {
            continue;
        }
        for e in set["emoticons"].as_array().into_iter().flatten() {
            let Some(name) = e["name"].as_str() else {
                continue;
            };
            let urls = if e["animated"].is_object() {
                &e["animated"]
            } else {
                &e["urls"]
            };
            if let Some(url) = urls["1"].as_str().or(urls["2"].as_str()) {
                map.insert(name.to_string(), https(url));
            }
        }
    }
}

fn bttv_into(map: &mut EmoteMap, v: &Value) {
    let lists = if v.is_array() {
        vec![v]
    } else {
        vec![&v["channelEmotes"], &v["sharedEmotes"]]
    };
    for e in lists.into_iter().filter_map(Value::as_array).flatten() {
        if let (Some(id), Some(code)) = (e["id"].as_str(), e["code"].as_str()) {
            // .webp is served animated for animated emotes and static otherwise.
            map.insert(
                code.to_string(),
                format!("https://cdn.betterttv.net/emote/{id}/1x.webp"),
            );
        }
    }
}

fn seventv_into(map: &mut EmoteMap, set: &Value) {
    for e in set["emotes"].as_array().into_iter().flatten() {
        if let (Some(name), Some(host)) = (e["name"].as_str(), e["data"]["host"]["url"].as_str()) {
            map.insert(name.to_string(), format!("{}/1x.webp", https(host)));
        }
    }
}

struct Irc<'a> {
    tags: Vec<(&'a str, String)>,
    prefix: Option<&'a str>,
    command: &'a str,
    trailing: Option<&'a str>,
}

impl<'a> Irc<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let mut rest = line;
        let mut tags = Vec::new();
        if let Some(r) = rest.strip_prefix('@') {
            let (t, r) = r.split_once(' ')?;
            for kv in t.split(';') {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                tags.push((k, unescape_tag(v)));
            }
            rest = r;
        }
        let mut prefix = None;
        if let Some(r) = rest.strip_prefix(':') {
            let (p, r) = r.split_once(' ')?;
            prefix = Some(p);
            rest = r;
        }
        let (head, trailing) = match rest.split_once(" :") {
            Some((h, t)) => (h, Some(t)),
            None => (rest, None),
        };
        let command = head.split(' ').next()?;
        Some(Self {
            tags,
            prefix,
            command,
            trailing,
        })
    }

    fn tag(&self, key: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }
}

fn unescape_tag(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('s') => out.push(' '),
            Some(':') => out.push(';'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_forms() {
        assert_eq!(
            parse_channel("https://www.twitch.tv/xQc").as_deref(),
            Some("xqc")
        );
        assert_eq!(
            parse_channel("twitch.tv/some_one/videos").as_deref(),
            Some("some_one")
        );
        assert_eq!(parse_channel("twitch:#Name").as_deref(), Some("name"));
        assert_eq!(parse_channel("@LofiGirl"), None);
    }

    #[test]
    fn parses_privmsg_with_emotes_and_badges() {
        let line = r"@badges=moderator/1,subscriber/12;color=#1976D2;display-name=Fossa\sBot;emotes=1035663:0-3,10-13;id=abc-1;mod=1;tmi-sent-ts=1791339488176;user-id=237719657 :fossabot!fossabot@fossabot.tmi.twitch.tv PRIVMSG #xqc :xqcL hi 😀 xqcL!";
        let irc = Irc::parse(line).unwrap();
        assert_eq!(irc.command, "PRIVMSG");
        let m = chat_message(&irc, None, &EmoteMap::new()).unwrap();
        assert_eq!(m.author, "Fossa Bot");
        assert_eq!(m.channel_id, "237719657");
        assert!(m.is_mod && m.is_member && !m.is_owner);
        assert_eq!(
            m.profile_url.as_deref(),
            Some("https://www.twitch.tv/fossabot")
        );
        let url = "https://static-cdn.jtvnw.net/emoticons/v2/1035663/default/dark/1.0".to_string();
        assert_eq!(
            m.parts,
            vec![
                Part::Emote {
                    url: url.clone(),
                    alt: "xqcL".into()
                },
                Part::Text(" hi 😀 ".into()),
                Part::Emote {
                    url,
                    alt: "xqcL".into()
                },
                Part::Text("!".into()),
            ]
        );
    }

    #[test]
    fn action_and_bits() {
        let line =
            "@bits=100;display-name=A;id=x;user-id=1 :a!a@a PRIVMSG #c :\u{1}ACTION waves\u{1}";
        let m = chat_message(&Irc::parse(line).unwrap(), None, &EmoteMap::new()).unwrap();
        assert_eq!(m.text, "waves");
        assert_eq!(m.superchat.as_deref(), Some("100 bits"));
    }

    #[test]
    fn third_party_word_emotes() {
        let mut map = EmoteMap::new();
        map.insert("GAMBA".into(), "https://7tv/g".into());
        let parts = word_emotes(vec![Part::Text("GAMBA time GAMBA! GAMBA".into())], &map);
        assert_eq!(
            parts,
            vec![
                Part::Emote {
                    url: "https://7tv/g".into(),
                    alt: "GAMBA".into()
                },
                Part::Text(" time GAMBA! ".into()),
                Part::Emote {
                    url: "https://7tv/g".into(),
                    alt: "GAMBA".into()
                },
            ]
        );
    }

    #[test]
    fn emote_set_parsers() {
        let mut map = EmoteMap::new();
        let ffz = serde_json::json!({ "default_sets": [3], "sets": {
            "3": { "emoticons": [{ "name": "A", "urls": { "1": "//f/1", "2": "//f/2" } },
                                 { "name": "B", "urls": { "2": "//f/b" }, "animated": { "2": "//f/b/anim" } }] },
            "9": { "emoticons": [{ "name": "Hidden", "urls": { "2": "//x" } }] } } });
        let defaults = ffz["default_sets"].as_array().cloned().unwrap();
        ffz_into(&mut map, &ffz, Some(&defaults));
        bttv_into(
            &mut map,
            &serde_json::json!({ "channelEmotes": [{ "id": "i1", "code": "C" }], "sharedEmotes": [] }),
        );
        seventv_into(
            &mut map,
            &serde_json::json!({ "emotes": [{ "name": "A", "data": { "host": { "url": "//cdn.7tv.app/emote/z" } } }] }),
        );
        assert_eq!(map["A"], "https://cdn.7tv.app/emote/z/1x.webp"); // 7TV overrides FFZ
        assert_eq!(map["B"], "https://f/b/anim");
        assert_eq!(map["C"], "https://cdn.betterttv.net/emote/i1/1x.webp");
        assert!(!map.contains_key("Hidden"));
    }

    #[test]
    fn moderation_commands() {
        let ban = Irc::parse("@room-id=1;target-user-id=42 :tmi.twitch.tv CLEARCHAT #c :baduser")
            .unwrap();
        assert_eq!(
            (ban.command, ban.tag("target-user-id")),
            ("CLEARCHAT", Some("42"))
        );
        let del = Irc::parse("@login=x;target-msg-id=m1 :tmi.twitch.tv CLEARMSG #c :text").unwrap();
        assert_eq!(del.tag("target-msg-id"), Some("m1"));
        assert_eq!(
            Irc::parse("PING :tmi.twitch.tv").unwrap().trailing,
            Some("tmi.twitch.tv")
        );
    }
}
