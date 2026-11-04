//! Chat sources. Each runs on its own thread and streams `SourceEvent`s back.

pub mod channel;
mod twitch;

use crate::config::SourceKind;
use crate::model::{self, ChatMsg, Part, SourceEvent};
use eframe::egui;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
// Skips the EU cookie-consent interstitial.
const CONSENT_COOKIE: &str = "CONSENT=YES+1; SOCS=CAI";

/// Floor so a bad config value can't hammer YouTube.
const MIN_POLL_MS: u64 = 250;

pub struct SourceHandle {
    stop: Arc<AtomicBool>,
}

impl SourceHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for SourceHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct StartParams {
    pub kind: SourceKind,
    pub target: String,
    pub api_key: String,
    pub api_min_poll_secs: u64,
    pub innertube_poll_ms: u64,
}

pub fn start(
    sid: u64,
    params: StartParams,
    tx: Sender<(u64, SourceEvent)>,
    ctx: egui::Context,
) -> Result<SourceHandle, String> {
    let target = params.target.trim().to_string();
    if target.is_empty() {
        return Err("Enter a YouTube URL/@channel or a twitch.tv/ link".into());
    }
    let is_twitch = twitch::parse_channel(&target).is_some();
    if !is_twitch && params.kind == SourceKind::Api && params.api_key.trim().is_empty() {
        return Err("API mode needs an API key (Settings)".into());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let handle = SourceHandle { stop: stop.clone() };

    std::thread::spawn(move || {
        let out = Out { sid, tx, ctx, stop };
        let result = run(&params, &target, &out);
        let (reason, error) = match result {
            Ok(()) if out.stopped() => ("Disconnected".to_string(), false),
            Ok(()) => ("Stream chat ended".to_string(), false),
            Err(e) => (e, true),
        };
        out.send(SourceEvent::Ended { reason, error });
    });
    Ok(handle)
}

struct Out {
    sid: u64,
    tx: Sender<(u64, SourceEvent)>,
    ctx: egui::Context,
    stop: Arc<AtomicBool>,
}

impl Out {
    fn send(&self, mut ev: SourceEvent) {
        if let SourceEvent::Message(m) = &mut ev {
            m.source = self.sid;
        }
        let _ = self.tx.send((self.sid, ev));
        self.ctx.request_repaint();
    }

    fn status(&self, s: impl Into<String>) {
        self.send(SourceEvent::Status(s.into()));
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Sleeps in short slices so Disconnect is responsive. Returns false if stopped.
    fn sleep(&self, d: Duration) -> bool {
        let step = Duration::from_millis(100);
        let mut left = d;
        while !left.is_zero() {
            if self.stopped() {
                return false;
            }
            let s = left.min(step);
            std::thread::sleep(s);
            left -= s;
        }
        !self.stopped()
    }
}

fn run(p: &StartParams, target: &str, out: &Out) -> Result<(), String> {
    if let Some(channel) = twitch::parse_channel(target) {
        return twitch::run(&channel, out);
    }
    let http = Http::new();
    let video_id = match parse_video_id(target) {
        Some(id) => id,
        None => {
            out.status(format!("Finding live stream for {target}…"));
            resolve_channel_live(&http, target)?
        }
    };
    let title = channel_name(&http, &video_id).unwrap_or_default();
    out.send(SourceEvent::Resolved {
        video_id: video_id.clone(),
        title,
    });
    match p.kind {
        SourceKind::Innertube => {
            let poll = Duration::from_millis(p.innertube_poll_ms.max(MIN_POLL_MS));
            innertube::run(&http, &video_id, poll, out)
        }
        SourceKind::Api => api::run(&http, &video_id, p.api_key.trim(), p.api_min_poll_secs, out),
    }
}

/// True for "@handle", channel URLs, and UC… ids: things that name a channel
/// rather than one video or a Twitch stream.
pub fn is_youtube_channel(target: &str) -> bool {
    let t = target.trim();
    !t.is_empty() && twitch::parse_channel(t).is_none() && parse_video_id(t).is_none()
}

/// Lists a channel's live and upcoming streams on a background thread.
/// The result is delivered through `done` and the UI is woken up.
pub fn discover_channel(
    target: String,
    ctx: egui::Context,
    done: Sender<(String, Result<channel::ChannelStreams, String>)>,
) {
    std::thread::spawn(move || {
        let result = channel::list(&Http::new(), &target);
        let _ = done.send((target, result));
        ctx.request_repaint();
    });
}

/// Accepts a bare 11-char ID or any youtube.com / youtu.be URL form.
pub fn parse_video_id(input: &str) -> Option<String> {
    let s = input.trim();
    let is_id = |t: &str| {
        t.len() == 11
            && t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if is_id(s) {
        return Some(s.to_string());
    }
    let take_id = |rest: &str| {
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        is_id(&id).then_some(id)
    };
    for marker in ["v=", "youtu.be/", "/live/", "/shorts/", "/embed/"] {
        if let Some(pos) = s.find(marker)
            && let Some(id) = take_id(&s[pos + marker.len()..])
        {
            return Some(id);
        }
    }
    None
}

/// "@handle", a channel URL, or a UC… id -> the video ID of its current live stream.
fn resolve_channel_live(http: &Http, target: &str) -> Result<String, String> {
    let t = target.trim().trim_end_matches('/');
    let base = if t.starts_with("http") {
        t.trim_end_matches("/live")
            .trim_end_matches("/streams")
            .to_string()
    } else if t.starts_with("UC") && t.len() == 24 {
        format!("https://www.youtube.com/channel/{t}")
    } else {
        format!("https://www.youtube.com/@{}", t.trim_start_matches('@'))
    };
    let html = http.get_text(&format!("{base}/live"))?;
    let canonical = between(&html, "<link rel=\"canonical\" href=\"", "\"").unwrap_or_default();
    parse_video_id(canonical)
        .filter(|_| canonical.contains("watch"))
        .ok_or_else(|| format!("{target} doesn't appear to be live right now"))
}

/// Channel display name via oEmbed; only used to label the stream.
fn channel_name(http: &Http, video_id: &str) -> Option<String> {
    let (status, v) = http
        .get_json(&format!(
            "https://www.youtube.com/oembed?url=https://www.youtube.com/watch?v={video_id}&format=json"
        ))
        .ok()?;
    (status < 400)
        .then(|| v["author_name"].as_str().map(str::to_string))
        .flatten()
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)?;
    Some(&s[i..i + j])
}

struct Http {
    agent: ureq::Agent,
}

impl Http {
    fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent }
    }

    fn get_text(&self, url: &str) -> Result<String, String> {
        let mut resp = self
            .agent
            .get(url)
            .header("User-Agent", UA)
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", CONSENT_COOKIE)
            .call()
            .map_err(|e| format!("Network error: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(20 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| format!("Read error: {e}"))?;
        if status >= 400 {
            return Err(format!("HTTP {status} from {url}"));
        }
        Ok(body)
    }

    /// Returns (status, json) and leaves error interpretation to the caller.
    fn get_json(&self, url: &str) -> Result<(u16, Value), String> {
        let mut resp = self
            .agent
            .get(url)
            .header("User-Agent", UA)
            .call()
            .map_err(|e| format!("Network error: {e}"))?;
        let status = resp.status().as_u16();
        let v = resp.body_mut().read_json::<Value>().unwrap_or(Value::Null);
        Ok((status, v))
    }

    fn post_json(&self, url: &str, body: &Value) -> Result<(u16, Value), String> {
        let mut resp = self
            .agent
            .post(url)
            .header("User-Agent", UA)
            .header("Cookie", CONSENT_COOKIE)
            .header("Origin", "https://www.youtube.com")
            .send_json(body)
            .map_err(|e| format!("Network error: {e}"))?;
        let status = resp.status().as_u16();
        let v = resp
            .body_mut()
            .with_config()
            .limit(20 * 1024 * 1024)
            .read_json::<Value>()
            .unwrap_or(Value::Null);
        Ok((status, v))
    }
}

/// The endpoint youtube.com's own live chat uses.
mod innertube {
    use super::*;

    const MAX_FAILURES: u32 = 5;

    pub fn run(http: &Http, video_id: &str, poll: Duration, out: &Out) -> Result<(), String> {
        out.status("Loading chat page…");
        let html = http.get_text(&format!(
            "https://www.youtube.com/live_chat?is_popout=1&v={video_id}"
        ))?;

        let api_key = between(&html, "\"INNERTUBE_API_KEY\":\"", "\"").map(str::to_string);
        let client_version = between(&html, "\"INNERTUBE_CLIENT_VERSION\":\"", "\"")
            .unwrap_or("2.20241001.00.00")
            .to_string();
        let initial = extract_json_after(&html, "ytInitialData")
            .ok_or("No live chat on this video (not live, chat disabled, or members-only)")?;

        let renderer = &initial["contents"]["liveChatRenderer"];
        if renderer.is_null() {
            let msg = initial["contents"]["messageRenderer"]["text"]["runs"][0]["text"]
                .as_str()
                .unwrap_or("Live chat unavailable for this video");
            return Err(msg.to_string());
        }

        // "Live chat" (everything), not the default "Top chat" (filtered by YouTube).
        // No fallback: silently showing Top chat would hide messages from a moderator.
        let views = renderer["header"]["liveChatHeaderRenderer"]["viewSelector"]["sortFilterSubMenuRenderer"]
            ["subMenuItems"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let live_view = views
            .iter()
            .find(|v| v["title"].as_str() == Some("Live chat"))
            .or_else(|| views.get(1));
        let mut continuation = live_view
            .and_then(|v| v["continuation"]["reloadContinuationData"]["continuation"].as_str())
            .map(str::to_string)
            .ok_or("Couldn't find the Live chat view (only Top chat is available)")?;

        let url = match &api_key {
            Some(k) => format!(
                "https://www.youtube.com/youtubei/v1/live_chat/get_live_chat?key={k}&prettyPrint=false"
            ),
            None => "https://www.youtube.com/youtubei/v1/live_chat/get_live_chat?prettyPrint=false"
                .into(),
        };
        out.status(format!("Connected (InnerTube, Live chat) · {video_id}"));

        let mut failures = 0;
        loop {
            if out.stopped() {
                return Ok(());
            }
            let body = json!({
                "context": { "client": { "clientName": "WEB", "clientVersion": client_version, "hl": "en" } },
                "continuation": continuation,
            });
            let (status, v) = match http.post_json(&url, &body) {
                Ok(r) => r,
                Err(e) => {
                    failures += 1;
                    if failures >= MAX_FAILURES {
                        return Err(e);
                    }
                    out.status(format!("{e} — retrying ({failures}/{MAX_FAILURES})"));
                    if !out.sleep(Duration::from_secs(2 * failures as u64)) {
                        return Ok(());
                    }
                    continue;
                }
            };
            if status >= 400 {
                failures += 1;
                if failures >= MAX_FAILURES {
                    return Err(format!("YouTube returned HTTP {status}"));
                }
                out.status(format!(
                    "HTTP {status} — retrying ({failures}/{MAX_FAILURES})"
                ));
                if !out.sleep(Duration::from_secs(2 * failures as u64)) {
                    return Ok(());
                }
                continue;
            }
            if failures > 0 {
                failures = 0;
                out.status(format!("Connected (InnerTube, Live chat) · {video_id}"));
            }

            let lcc = &v["continuationContents"]["liveChatContinuation"];
            if let Some(actions) = lcc["actions"].as_array() {
                for a in actions {
                    if let Some(ev) = parse_action(a) {
                        out.send(ev);
                    }
                }
            }
            let Some((next, timeout_ms, pushed)) = next_continuation(&lcc["continuations"]) else {
                return Ok(()); // no continuation = stream over
            };
            continuation = next;
            // The browser gets "invalidation" updates pushed instantly and only falls
            // back to the ~10s timeoutMs; we have no push channel, so poll on our own clock.
            let wait = if pushed {
                poll
            } else {
                Duration::from_millis(timeout_ms).clamp(poll, Duration::from_secs(10))
            };
            if !out.sleep(wait) {
                return Ok(());
            }
        }
    }

    /// (token, YouTube's suggested wait, whether it's a push-style continuation)
    fn next_continuation(conts: &Value) -> Option<(String, u64, bool)> {
        let c = conts.as_array()?.first()?;
        for key in [
            "invalidationContinuationData",
            "timedContinuationData",
            "reloadContinuationData",
        ] {
            if let Some(token) = c[key]["continuation"].as_str() {
                let timeout = c[key]["timeoutMs"].as_u64().unwrap_or(2000);
                return Some((
                    token.to_string(),
                    timeout,
                    key == "invalidationContinuationData",
                ));
            }
        }
        None
    }

    fn parse_action(a: &Value) -> Option<SourceEvent> {
        if let Some(item) = a.get("addChatItemAction").map(|x| &x["item"]) {
            return parse_item(item).map(SourceEvent::Message);
        }
        // YouTube has used both the "mark" and "remove" forms for deletes and bans.
        for key in ["markChatItemAsDeletedAction", "removeChatItemAction"] {
            if let Some(id) = a[key]["targetItemId"].as_str() {
                return Some(SourceEvent::Deleted(id.to_string()));
            }
        }
        for key in [
            "markChatItemsByAuthorAsDeletedAction",
            "removeChatItemByAuthorAction",
        ] {
            if let Some(ch) = a[key]["externalChannelId"].as_str() {
                return Some(SourceEvent::DeletedByAuthor(ch.to_string()));
            }
        }
        // Swaps a message in place: a held message being approved (show it),
        // or a message being replaced by a "removed" placeholder (drop it).
        if let Some(r) = a.get("replaceChatItemAction") {
            return match parse_item(&r["replacementItem"]) {
                Some(m) => Some(SourceEvent::Message(m)),
                None => r["targetItemId"]
                    .as_str()
                    .map(|id| SourceEvent::Deleted(id.to_string())),
            };
        }
        None
    }

    fn parse_item(item: &Value) -> Option<ChatMsg> {
        let (r, superchat) = if let Some(r) = item.get("liveChatTextMessageRenderer") {
            (r, None)
        } else if let Some(r) = item.get("liveChatPaidMessageRenderer") {
            (r, Some(text_of(&r["purchaseAmountText"])))
        } else {
            let r = item.get("liveChatPaidStickerRenderer")?;
            (
                r,
                Some(format!("{} sticker", text_of(&r["purchaseAmountText"]))),
            )
        };

        let (text, parts) = runs(&r["message"]);
        let mut msg = ChatMsg {
            source: 0,
            profile_url: None,
            id: r["id"].as_str()?.to_string(),
            author: text_of(&r["authorName"]),
            channel_id: r["authorExternalChannelId"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            text,
            parts,
            time: r["timestampUsec"]
                .as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .map(model::local_time_from_micros)
                .unwrap_or_else(model::now_time),
            is_owner: false,
            is_mod: false,
            is_member: false,
            is_verified: false,
            superchat,
            deleted: false,
            hits: Vec::new(),
            name_flagged: false,
            room_id: None,
            mod_status: Default::default(),
            yt_menu: r["contextMenuEndpoint"]["liveChatItemContextMenuEndpoint"]["params"]
                .as_str()
                .map(str::to_string),
        };
        if let Some(badges) = r["authorBadges"].as_array() {
            for b in badges {
                let b = &b["liveChatAuthorBadgeRenderer"];
                match b["icon"]["iconType"].as_str() {
                    Some("OWNER") => msg.is_owner = true,
                    Some("MODERATOR") => msg.is_mod = true,
                    Some("VERIFIED") => msg.is_verified = true,
                    _ if b.get("customThumbnail").is_some() => msg.is_member = true,
                    _ => {}
                }
            }
        }
        Some(msg)
    }

    fn text_of(v: &Value) -> String {
        v["simpleText"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| runs(v).0)
    }

    /// (plain text, display parts). In the plain text, emoji keep their
    /// :shortcut: so word rules don't match inside them.
    fn runs(v: &Value) -> (String, Vec<Part>) {
        let mut text = String::new();
        let mut parts: Vec<Part> = Vec::new();
        let Some(runs) = v["runs"].as_array() else {
            return (text, parts);
        };
        for run in runs {
            if let Some(t) = run["text"].as_str() {
                text.push_str(t);
                match parts.last_mut() {
                    Some(Part::Text(prev)) => prev.push_str(t),
                    _ => parts.push(Part::Text(t.to_string())),
                }
            } else if let Some(e) = run.get("emoji") {
                let is_custom = e["isCustomEmoji"].as_bool().unwrap_or(false);
                let id = e["emojiId"].as_str().unwrap_or_default();
                let shortcut = e["shortcuts"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(Value::as_str);
                let alt = if !is_custom && !id.is_empty() && !id.is_ascii() {
                    id.to_string() // standard unicode emoji
                } else {
                    shortcut.unwrap_or_default().to_string()
                };
                text.push_str(&alt);
                match emote_url(e) {
                    Some(url) => parts.push(Part::Emote { url, alt }),
                    None => parts.push(Part::Text(alt)),
                }
            }
        }
        (text, parts)
    }

    /// Largest thumbnail (YouTube lists them small to large).
    fn emote_url(e: &Value) -> Option<String> {
        let url = e["image"]["thumbnails"].as_array()?.last()?["url"].as_str()?;
        Some(if url.starts_with("//") {
            format!("https:{url}")
        } else {
            url.to_string()
        })
    }

    /// Finds `marker = {...}` in the page and parses just that JSON value.
    pub(super) fn extract_json_after(html: &str, marker: &str) -> Option<Value> {
        let mut from = 0;
        while let Some(pos) = html[from..].find(marker) {
            let after = from + pos + marker.len();
            let rest = &html[after..];
            if let Some(brace) = rest.find('{') {
                let prefix = rest[..brace].trim();
                if prefix == "=" || prefix == "\"] =" || prefix == "\"]=" {
                    let mut stream =
                        serde_json::Deserializer::from_str(&rest[brace..]).into_iter::<Value>();
                    if let Some(Ok(v)) = stream.next() {
                        return Some(v);
                    }
                }
            }
            from = after;
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_text_message_with_badges_and_emoji() {
            let item = json!({ "liveChatTextMessageRenderer": {
                "id": "abc",
                "authorName": { "simpleText": "Bob" },
                "authorExternalChannelId": "UC123",
                "timestampUsec": "1700000000000000",
                "message": { "runs": [
                    { "text": "hi " },
                    { "emoji": { "emojiId": "😀", "shortcuts": [":grinning:"],
                        "image": { "thumbnails": [{ "url": "https://e/72.png" }] } } },
                    { "emoji": { "emojiId": "UCx/abc", "isCustomEmoji": true, "shortcuts": [":_custom:"],
                        "image": { "thumbnails": [{ "url": "//c/24" }, { "url": "//c/48" }] } } }
                ]},
                "authorBadges": [
                    { "liveChatAuthorBadgeRenderer": { "icon": { "iconType": "MODERATOR" } } },
                    { "liveChatAuthorBadgeRenderer": { "customThumbnail": {} } }
                ]
            }});
            let m = parse_item(&item).unwrap();
            assert_eq!(m.author, "Bob");
            assert_eq!(m.text, "hi 😀:_custom:");
            assert_eq!(
                m.parts,
                vec![
                    Part::Text("hi ".into()),
                    Part::Emote {
                        url: "https://e/72.png".into(),
                        alt: "😀".into()
                    },
                    Part::Emote {
                        url: "https://c/48".into(),
                        alt: ":_custom:".into()
                    },
                ]
            );
            assert!(m.is_mod && m.is_member && !m.is_owner);
        }

        #[test]
        fn extracts_initial_data() {
            let html = r#"<script>window["ytInitialData"] = {"a":{"b":"x};y"}};var z=1;</script>"#;
            assert_eq!(
                extract_json_after(html, "ytInitialData").unwrap()["a"]["b"],
                "x};y"
            );
            let html2 = r#"var ytInitialData = {"k":1};"#;
            assert_eq!(extract_json_after(html2, "ytInitialData").unwrap()["k"], 1);
        }

        #[test]
        fn parses_deletions() {
            for key in ["markChatItemAsDeletedAction", "removeChatItemAction"] {
                let a = json!({ key: { "targetItemId": "id1" } });
                assert!(matches!(parse_action(&a), Some(SourceEvent::Deleted(id)) if id == "id1"));
            }
            for key in [
                "markChatItemsByAuthorAsDeletedAction",
                "removeChatItemByAuthorAction",
            ] {
                let a = json!({ key: { "externalChannelId": "UCx" } });
                assert!(
                    matches!(parse_action(&a), Some(SourceEvent::DeletedByAuthor(c)) if c == "UCx")
                );
            }
            let placeholder = json!({ "replaceChatItemAction": { "targetItemId": "id2",
                "replacementItem": { "liveChatPlaceholderItemRenderer": { "id": "id2" } } } });
            assert!(
                matches!(parse_action(&placeholder), Some(SourceEvent::Deleted(id)) if id == "id2")
            );
        }
    }
}

/// Official YouTube Data API v3.
mod api {
    use super::*;

    const BASE: &str = "https://www.googleapis.com/youtube/v3";

    pub fn run(
        http: &Http,
        video_id: &str,
        key: &str,
        min_poll_secs: u64,
        out: &Out,
    ) -> Result<(), String> {
        out.status("Looking up live chat ID (API)…");
        let (status, v) = http.get_json(&format!(
            "{BASE}/videos?part=liveStreamingDetails&id={video_id}&key={}",
            enc(key)
        ))?;
        check(status, &v)?;
        let chat_id = v["items"][0]["liveStreamingDetails"]["activeLiveChatId"]
            .as_str()
            .ok_or("Video isn't live or has no active chat")?
            .to_string();
        out.status(format!("Connected (API) · {video_id}"));

        let min_poll = Duration::from_secs(min_poll_secs.max(1));
        let mut page_token: Option<String> = None;
        let mut failures = 0;
        loop {
            if out.stopped() {
                return Ok(());
            }
            let mut url = format!(
                "{BASE}/liveChat/messages?liveChatId={}&part=snippet,authorDetails&maxResults=2000&key={}",
                enc(&chat_id),
                enc(key)
            );
            if let Some(t) = &page_token {
                url.push_str("&pageToken=");
                url.push_str(&enc(t));
            }
            let (status, v) = match http.get_json(&url) {
                Ok(r) => r,
                Err(e) => {
                    failures += 1;
                    if failures >= 5 {
                        return Err(e);
                    }
                    out.status(format!("{e} — retrying ({failures}/5)"));
                    if !out.sleep(Duration::from_secs(3 * failures)) {
                        return Ok(());
                    }
                    continue;
                }
            };
            if let Err(e) = check(status, &v) {
                if e.contains("liveChatEnded") || e.contains("ended") {
                    return Ok(());
                }
                return Err(e);
            }
            failures = 0;

            if let Some(items) = v["items"].as_array() {
                for it in items {
                    if let Some(ev) = parse_item(it) {
                        out.send(ev);
                    }
                }
            }
            if v["offlineAt"].is_string() {
                return Ok(());
            }
            page_token = v["nextPageToken"].as_str().map(str::to_string);
            let suggested =
                Duration::from_millis(v["pollingIntervalMillis"].as_u64().unwrap_or(5000));
            if !out.sleep(suggested.max(min_poll)) {
                return Ok(());
            }
        }
    }

    fn check(status: u16, v: &Value) -> Result<(), String> {
        if status < 400 {
            return Ok(());
        }
        let reason = v["error"]["errors"][0]["reason"].as_str().unwrap_or("");
        let message = v["error"]["message"].as_str().unwrap_or("");
        Err(match reason {
            "quotaExceeded" | "rateLimitExceeded" => {
                "API quota exceeded for today — switch to InnerTube or wait for reset (midnight Pacific)".into()
            }
            "liveChatEnded" => "liveChatEnded".into(),
            "keyInvalid" | "badRequest" if message.contains("API key") => "API key is invalid".into(),
            _ => format!("API error {status}: {reason} {message}").trim().to_string(),
        })
    }

    fn parse_item(it: &Value) -> Option<SourceEvent> {
        let sn = &it["snippet"];
        let ad = &it["authorDetails"];
        match sn["type"].as_str()? {
            "messageDeletedEvent" => sn["messageDeletedDetails"]["deletedMessageId"]
                .as_str()
                .map(|id| SourceEvent::Deleted(id.to_string())),
            "userBannedEvent" => sn["userBannedDetails"]["bannedUserDetails"]["channelId"]
                .as_str()
                .map(|id| SourceEvent::DeletedByAuthor(id.to_string())),
            kind @ ("textMessageEvent" | "superChatEvent" | "superStickerEvent") => {
                let superchat = match kind {
                    "superChatEvent" => sn["superChatDetails"]["amountDisplayString"]
                        .as_str()
                        .map(str::to_string),
                    "superStickerEvent" => sn["superStickerDetails"]["amountDisplayString"]
                        .as_str()
                        .map(|a| format!("{a} sticker")),
                    _ => None,
                };
                // The API only gives plain text, so channel emotes stay as :name:.
                let text = sn["displayMessage"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Some(SourceEvent::Message(ChatMsg {
                    source: 0,
                    profile_url: None,
                    id: it["id"].as_str()?.to_string(),
                    author: ad["displayName"].as_str().unwrap_or_default().to_string(),
                    channel_id: ad["channelId"].as_str().unwrap_or_default().to_string(),
                    parts: vec![Part::Text(text.clone())],
                    text,
                    time: sn["publishedAt"]
                        .as_str()
                        .map(model::local_time_from_rfc3339)
                        .unwrap_or_else(model::now_time),
                    is_owner: ad["isChatOwner"].as_bool().unwrap_or(false),
                    is_mod: ad["isChatModerator"].as_bool().unwrap_or(false),
                    is_member: ad["isChatSponsor"].as_bool().unwrap_or(false),
                    is_verified: ad["isVerified"].as_bool().unwrap_or(false),
                    superchat,
                    deleted: false,
                    hits: Vec::new(),
                    name_flagged: false,
                    room_id: None,
                    mod_status: Default::default(),
                    yt_menu: None,
                }))
            }
            _ => None,
        }
    }

    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_id_forms() {
        let id = Some("dQw4w9WgXcQ".to_string());
        assert_eq!(parse_video_id("dQw4w9WgXcQ"), id);
        assert_eq!(
            parse_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1"),
            id
        );
        assert_eq!(parse_video_id("https://youtu.be/dQw4w9WgXcQ"), id);
        assert_eq!(
            parse_video_id("https://www.youtube.com/live/dQw4w9WgXcQ?si=x"),
            id
        );
        assert_eq!(parse_video_id("@somechannel"), None);
    }
}
