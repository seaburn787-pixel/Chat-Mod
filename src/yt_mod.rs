//! YouTube moderation through the same calls the website's ⋮ menu makes,
//! signed with the user's own login cookies. Runs only when the user clicks.
//!
//! Flow: a chat message carries `contextMenuEndpoint` params. Posting those to
//! `live_chat/get_item_context_menu` (logged in) returns the ⋮ menu YouTube
//! would show this account; each item holds the endpoint to call for it.

use crate::moderation::ModEvent;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ORIGIN: &str = "https://www.youtube.com";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// Saved YouTube login: the browser's Cookie header, pasted by the user.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct YoutubeAuth {
    pub cookies: String,
    /// Display only, e.g. "@SomeChannel".
    #[serde(default)]
    pub account: String,
}

/// One entry of the ⋮ menu.
#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: String,
    pub icon: String,
    /// Things Chat Mod can run. Several when YouTube asks for a choice
    /// (e.g. timeout lengths); empty when it has to be done on YouTube.
    pub actions: Vec<MenuAction>,
}

#[derive(Clone, Debug)]
pub struct MenuAction {
    /// Sub-choice label when an item has several actions.
    pub label: String,
    api_url: String,
    params: String,
    click_tracking: Option<String>,
}

/// The ⋮ item for a timeout (`ban == false`) or for hiding the user from
/// the channel, YouTube's ban (`ban == true`). Personal "Block" isn't a ban.
pub fn find_punish(items: &[MenuItem], ban: bool) -> Option<&MenuItem> {
    items.iter().find(|i| {
        let l = i.label.to_lowercase();
        if l.contains("unhide") || l.contains("unban") || l.contains("remove timeout") {
            return false;
        }
        if ban {
            l.contains("hide user") || l.starts_with("ban") || l.contains(" ban ")
        } else {
            i.icon == "HOURGLASS" || l.contains("timeout") || l.contains("time out")
        }
    })
}

impl MenuItem {
    pub fn is_delete(&self) -> bool {
        self.icon == "DELETE" || {
            let l = self.label.to_lowercase();
            l.contains("delete") || l.contains("remove message")
        }
    }
}

/// Values from the logged-in page that every request has to repeat.
#[derive(Default)]
struct Session {
    api_key: String,
    client_version: String,
    visitor: String,
    session_index: Option<String>,
    delegated: Option<String>,
    user_session: Option<String>,
}

struct Client {
    http: ureq::Agent,
    jar: Vec<(String, String)>,
    jar_changed: bool,
    session: Session,
}

impl Client {
    fn new(cookies: &str) -> Result<Self, String> {
        let jar = parse_cookie_header(cookies);
        if !jar
            .iter()
            .any(|(k, _)| k == "SAPISID" || k == "__Secure-3PAPISID")
        {
            return Err(
                "That doesn't look like a logged-in YouTube cookie (no SAPISID). \
                        Copy the whole cookie value again."
                    .into(),
            );
        }
        Ok(Self {
            http: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(20)))
                .http_status_as_error(false)
                .build()
                .into(),
            jar,
            jar_changed: false,
            session: Session::default(),
        })
    }

    fn cookie(&self, name: &str) -> Option<&str> {
        self.jar
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn cookie_header(&self) -> String {
        self.jar
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Google rotates some cookies in responses; keep ours in step so the
    /// pasted login lasts longer.
    fn absorb(&mut self, resp: &ureq::http::Response<ureq::Body>) {
        for h in resp.headers().get_all("set-cookie") {
            let Ok(h) = h.to_str() else { continue };
            let Some((name, rest)) = h.split_once('=') else {
                continue;
            };
            let value = rest.split(';').next().unwrap_or_default().trim();
            let name = name.trim();
            if value.is_empty() || value == "EXPIRED" {
                continue;
            }
            match self.jar.iter_mut().find(|(k, _)| k == name) {
                Some((_, v)) if v != value => {
                    *v = value.to_string();
                    self.jar_changed = true;
                }
                Some(_) => {}
                None => {
                    self.jar.push((name.to_string(), value.to_string()));
                    self.jar_changed = true;
                }
            }
        }
    }

    /// Loads the logged-in home page for the per-session values.
    fn start_session(&mut self) -> Result<(), String> {
        let mut resp = self
            .http
            .get(ORIGIN)
            .header("User-Agent", UA)
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", &self.cookie_header())
            .call()
            .map_err(|e| format!("Network error: {e}"))?;
        self.absorb(&resp);
        let html = resp
            .body_mut()
            .with_config()
            .limit(20 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| format!("Network error: {e}"))?;
        if !html.contains("\"LOGGED_IN\":true") {
            return Err("YouTube says this login isn't signed in. Copy a fresh cookie.".into());
        }
        let s = &mut self.session;
        s.api_key = cfg_str(&html, "INNERTUBE_API_KEY").unwrap_or_default();
        s.client_version =
            cfg_str(&html, "INNERTUBE_CLIENT_VERSION").unwrap_or_else(|| "2.20261028.01.00".into());
        s.visitor = cfg_str(&html, "VISITOR_DATA").unwrap_or_default();
        s.session_index = cfg_num(&html, "SESSION_INDEX");
        s.delegated = cfg_str(&html, "DELEGATED_SESSION_ID").filter(|x| !x.is_empty());
        if let Some(sync) = cfg_str(&html, "DATASYNC_ID") {
            let (first, second) = sync.split_once("||").unwrap_or((&sync, ""));
            if second.is_empty() {
                s.user_session = Some(first.to_string()).filter(|x| !x.is_empty());
            } else {
                s.delegated.get_or_insert_with(|| first.to_string());
                s.user_session = Some(second.to_string());
            }
        }
        Ok(())
    }

    fn context(&self, click_tracking: Option<&str>) -> Value {
        let mut ctx = json!({
            "client": {
                "clientName": "WEB",
                "clientVersion": self.session.client_version,
                "hl": "en",
                "visitorData": self.session.visitor,
            },
            "user": {},
        });
        if let Some(d) = &self.session.delegated {
            ctx["user"]["onBehalfOfUser"] = json!(d);
        }
        if let Some(c) = click_tracking {
            ctx["clickTracking"] = json!({ "clickTrackingParams": c });
        }
        ctx
    }

    /// Authorization header the website sends, from the SAPISID cookies.
    fn authorization(&self) -> String {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
            .to_string();
        let sapisid = self
            .cookie("SAPISID")
            .or_else(|| self.cookie("__Secure-3PAPISID"));
        let schemes = [
            ("SAPISIDHASH", sapisid),
            ("SAPISID1PHASH", self.cookie("__Secure-1PAPISID")),
            ("SAPISID3PHASH", self.cookie("__Secure-3PAPISID")),
        ];
        schemes
            .iter()
            .filter_map(|(scheme, sid)| {
                sid.map(|sid| sid_hash(scheme, sid, &ts, self.session.user_session.as_deref()))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn post(&mut self, path_and_query: &str, body: Value) -> Result<Value, String> {
        let sep = if path_and_query.contains('?') {
            '&'
        } else {
            '?'
        };
        let mut url = format!("{ORIGIN}{path_and_query}{sep}prettyPrint=false");
        if !self.session.api_key.is_empty() {
            url.push_str(&format!("&key={}", self.session.api_key));
        }
        let mut req = self
            .http
            .post(&url)
            .header("User-Agent", UA)
            .header("Content-Type", "application/json")
            .header("Origin", ORIGIN)
            .header("X-Origin", ORIGIN)
            .header("Cookie", &self.cookie_header())
            .header("Authorization", &self.authorization())
            .header("X-Youtube-Client-Name", "1")
            .header("X-Youtube-Client-Version", &self.session.client_version)
            .header("X-Youtube-Bootstrap-Logged-In", "true")
            .header(
                "X-Goog-AuthUser",
                self.session.session_index.as_deref().unwrap_or("0"),
            );
        if !self.session.visitor.is_empty() {
            req = req.header("X-Goog-Visitor-Id", &self.session.visitor);
        }
        if let Some(d) = &self.session.delegated {
            req = req.header("X-Goog-PageId", d);
        }
        let mut resp = req
            .send_json(body)
            .map_err(|e| format!("Network error: {e}"))?;
        self.absorb(&resp);
        let status = resp.status().as_u16();
        let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        if status == 401 || status == 403 {
            return Err("YouTube rejected the login. Paste a fresh cookie in Settings.".into());
        }
        if status >= 400 || v.get("error").is_some() {
            let msg = v["error"]["message"].as_str().unwrap_or("request failed");
            return Err(format!("YouTube error {status}: {msg}"));
        }
        Ok(v)
    }

    fn account_name(&mut self) -> String {
        let body = json!({ "context": self.context(None) });
        let Ok(v) = self.post("/youtubei/v1/account/account_menu", body) else {
            return String::new();
        };
        let header = find_key(&v, "activeAccountHeaderRenderer");
        let name = header.map(|h| text(&h["accountName"])).unwrap_or_default();
        let handle = header
            .map(|h| text(&h["channelHandle"]))
            .unwrap_or_default();
        match (name.is_empty(), handle.is_empty()) {
            (false, false) => format!("{name} ({handle})"),
            (false, true) => name,
            _ => handle,
        }
    }

    fn menu(&mut self, menu_params: &str) -> Result<Vec<MenuItem>, String> {
        let path = format!(
            "/youtubei/v1/live_chat/get_item_context_menu?params={}&pbj=1",
            enc(menu_params)
        );
        let body = json!({ "context": self.context(None) });
        let v = self.post(&path, body)?;
        let items = parse_menu(&v);
        if items.is_empty() {
            return Err("YouTube returned an empty menu for this message. \
                        The login may have expired, or the message is gone."
                .into());
        }
        Ok(items)
    }

    fn run(&mut self, a: &MenuAction) -> Result<String, String> {
        let body = json!({
            "context": self.context(a.click_tracking.as_deref()),
            "params": a.params,
        });
        let v = self.post(&a.api_url, body)?;
        // Toast text YouTube shows after an action, if any.
        let toast = find_key(&v, "notificationActionRenderer")
            .map(|n| text(&n["responseText"]))
            .unwrap_or_default();
        Ok(if toast.is_empty() {
            "Done".into()
        } else {
            toast
        })
    }

    fn report_cookies(&self, tx: &Sender<ModEvent>) {
        if self.jar_changed {
            let _ = tx.send(ModEvent::YtCookiesUpdated(self.cookie_header()));
        }
    }
}

/// Checks a pasted cookie and looks up which account it belongs to.
pub fn check_login(cookies: String, tx: Sender<ModEvent>, wake: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let result = (|| {
            let mut c = Client::new(&cookies)?;
            c.start_session()?;
            let account = c.account_name();
            Ok(YoutubeAuth {
                cookies: c.cookie_header(),
                account,
            })
        })();
        let _ = tx.send(ModEvent::YtLogin(result));
        wake();
    });
}

/// Fetches the ⋮ menu for one message.
pub fn fetch_menu(
    cookies: String,
    menu_params: String,
    key: (u64, String),
    tx: Sender<ModEvent>,
    wake: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut client = None;
        let result = (|| {
            let c = client.insert(Client::new(&cookies)?);
            c.start_session()?;
            c.menu(&menu_params)
        })();
        if let Some(c) = &client {
            c.report_cookies(&tx);
        }
        let _ = tx.send(ModEvent::YtMenu { key, result });
        wake();
    });
}

/// Runs one item the user picked (and confirmed) from the ⋮ menu.
pub fn run_action(
    cookies: String,
    action: MenuAction,
    key: (u64, String),
    is_delete: bool,
    tx: Sender<ModEvent>,
    wake: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut client = None;
        let result = (|| {
            let c = client.insert(Client::new(&cookies)?);
            c.start_session()?;
            c.run(&action)
        })();
        if let Some(c) = &client {
            c.report_cookies(&tx);
        }
        let _ = tx.send(ModEvent::YtActionDone {
            key,
            is_delete,
            result,
        });
        wake();
    });
}

/// The Delete button: opens the menu, picks its delete item, runs it.
pub fn delete_message(
    cookies: String,
    menu_params: String,
    key: (u64, String),
    tx: Sender<ModEvent>,
    wake: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut client = None;
        let result = (|| {
            let c = client.insert(Client::new(&cookies)?);
            c.start_session()?;
            let items = c.menu(&menu_params)?;
            let item = items
                .iter()
                .find(|i| i.icon == "DELETE" && !i.actions.is_empty())
                .or_else(|| {
                    items
                        .iter()
                        .find(|i| i.is_delete() && !i.actions.is_empty())
                })
                .ok_or("No delete option for this message. Are you a moderator in this channel?")?;
            c.run(&item.actions[0])
        })();
        if let Some(c) = &client {
            c.report_cookies(&tx);
        }
        let _ = tx.send(ModEvent::YtActionDone {
            key,
            is_delete: true,
            result,
        });
        wake();
    });
}

fn sid_hash(scheme: &str, sid: &str, ts: &str, user_session: Option<&str>) -> String {
    let input = match user_session {
        Some(u) => format!("{u} {ts} {sid} {ORIGIN}"),
        None => format!("{ts} {sid} {ORIGIN}"),
    };
    let hash = sha1_smol::Sha1::from(input.as_bytes()).digest().to_string();
    match user_session {
        Some(_) => format!("{scheme} {ts}_{hash}_u"),
        None => format!("{scheme} {ts}_{hash}"),
    }
}

/// Accepts the raw Cookie header value, or with a leading "cookie:".
pub fn parse_cookie_header(s: &str) -> Vec<(String, String)> {
    let s = s.trim();
    let s = s
        .strip_prefix("cookie:")
        .or_else(|| s.strip_prefix("Cookie:"))
        .unwrap_or(s);
    s.split(';')
        .filter_map(|p| {
            let (k, v) = p.trim().split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

fn cfg_str(html: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = html.find(&pat)? + pat.len();
    let end = html[start..].find('"')?;
    Some(html[start..start + end].to_string())
}

fn cfg_num(html: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let start = html.find(&pat)? + pat.len();
    let digits: String = html[start..]
        .trim_start_matches('"')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty()).then_some(digits)
}

fn text(v: &Value) -> String {
    if let Some(s) = v["simpleText"].as_str() {
        return s.to_string();
    }
    if let Some(s) = v["content"].as_str() {
        return s.to_string();
    }
    v["runs"]
        .as_array()
        .map(|r| r.iter().filter_map(|x| x["text"].as_str()).collect())
        .unwrap_or_default()
}

fn find_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => m
            .get(key)
            .or_else(|| m.values().find_map(|x| find_key(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

pub(crate) fn parse_menu(v: &Value) -> Vec<MenuItem> {
    let root = v
        .get("liveChatItemContextMenuSupportedRenderers")
        .unwrap_or(v);
    let mut items = Vec::new();
    collect_items(root, &mut items);
    items
}

fn collect_items(v: &Value, out: &mut Vec<MenuItem>) {
    match v {
        Value::Object(m) => {
            for kind in ["menuServiceItemRenderer", "menuNavigationItemRenderer"] {
                if let Some(r) = m.get(kind) {
                    let label = text(&r["text"]);
                    if !label.is_empty() {
                        let mut actions = Vec::new();
                        collect_actions(r, &label, &mut actions);
                        actions.dedup_by(|a, b| a.params == b.params);
                        out.push(MenuItem {
                            label,
                            icon: r["icon"]["iconType"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            actions,
                        });
                    }
                    return;
                }
            }
            for x in m.values() {
                collect_items(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_items(x, out)),
        _ => {}
    }
}

/// Endpoints Chat Mod will call: POSTs under /youtubei/v1/live_chat/ that
/// carry params. Anything else (report forms, links) stays on YouTube.
fn collect_actions(v: &Value, label: &str, out: &mut Vec<MenuAction>) {
    match v {
        Value::Object(m) => {
            // Nested dialogs label their own options ("5 minutes", "Confirm").
            let here = ["text", "title", "label"]
                .iter()
                .map(|k| m.get(*k).map(text).unwrap_or_default())
                .find(|t| !t.is_empty())
                .unwrap_or_else(|| label.to_string());
            let meta = &v["commandMetadata"]["webCommandMetadata"];
            if let Some(api) = meta["apiUrl"].as_str()
                && api.starts_with("/youtubei/v1/live_chat/")
                && !api.contains("get_item_context_menu")
                && let Some(params) = m
                    .iter()
                    .filter(|(k, _)| k.ends_with("Endpoint") || k.ends_with("Command"))
                    .find_map(|(_, e)| e["params"].as_str())
            {
                out.push(MenuAction {
                    label: here,
                    api_url: api.to_string(),
                    params: params.to_string(),
                    click_tracking: m
                        .get("clickTrackingParams")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
                return;
            }
            for x in m.values() {
                collect_actions(x, &here, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_actions(x, label, out)),
        _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_hash_matches_known_value() {
        // sha1("1700000000 abc https://www.youtube.com")
        let h = sid_hash("SAPISIDHASH", "abc", "1700000000", None);
        let expect = sha1_smol::Sha1::from("1700000000 abc https://www.youtube.com")
            .digest()
            .to_string();
        assert_eq!(h, format!("SAPISIDHASH 1700000000_{expect}"));
        let u = sid_hash("SAPISID1PHASH", "abc", "1", Some("42"));
        assert!(u.starts_with("SAPISID1PHASH 1_") && u.ends_with("_u"));
    }

    #[test]
    fn cookie_paste_forms() {
        let j = parse_cookie_header("cookie: SID=a; SAPISID=b/c; X=1=2");
        assert_eq!(j[1], ("SAPISID".into(), "b/c".into()));
        assert_eq!(j[2], ("X".into(), "1=2".into()));
        assert!(Client::new("SID=a").is_err());
    }

    #[test]
    fn menu_parsing_finds_actions_and_dialog_choices() {
        let moderate = |p: &str| {
            json!({
                "clickTrackingParams": "ct",
                "commandMetadata": { "webCommandMetadata": { "sendPost": true, "apiUrl": "/youtubei/v1/live_chat/moderate" } },
                "moderateLiveChatEndpoint": { "params": p }
            })
        };
        let v = json!({ "liveChatItemContextMenuSupportedRenderers": { "menuRenderer": { "items": [
            { "menuServiceItemRenderer": { "text": { "runs": [{ "text": "Delete" }] },
                "icon": { "iconType": "DELETE" }, "serviceEndpoint": moderate("DEL") } },
            { "menuServiceItemRenderer": { "text": { "runs": [{ "text": "Put user in timeout" }] },
                "icon": { "iconType": "HOURGLASS" }, "serviceEndpoint": { "signalServiceEndpoint": { "actions": [
                    { "openPopupAction": { "popup": { "dialog": { "options": [
                        { "button": { "text": { "simpleText": "10 seconds" }, "serviceEndpoint": moderate("T10") } },
                        { "button": { "text": { "simpleText": "5 minutes" }, "serviceEndpoint": moderate("T300") } }
                    ] } } } }
                ] } } } },
            { "menuServiceItemRenderer": { "text": { "runs": [{ "text": "Report" }] },
                "icon": { "iconType": "FLAG" }, "serviceEndpoint": {
                    "commandMetadata": { "webCommandMetadata": { "sendPost": true, "apiUrl": "/youtubei/v1/flag/get_form" } },
                    "getReportFormEndpoint": { "params": "R" } } } }
        ] } } });
        let items = parse_menu(&v);
        assert_eq!(items.len(), 3);
        assert!(items[0].is_delete());
        assert_eq!(items[0].actions[0].params, "DEL");
        assert_eq!(items[0].actions[0].click_tracking.as_deref(), Some("ct"));
        let t: Vec<_> = items[1]
            .actions
            .iter()
            .map(|a| (a.label.as_str(), a.params.as_str()))
            .collect();
        assert_eq!(t, [("10 seconds", "T10"), ("5 minutes", "T300")]);
        assert!(items[2].actions.is_empty(), "report forms stay on YouTube");
        assert!(!items[2].is_delete());
    }

    #[test]
    fn finds_timeout_and_hide_items() {
        let item = |label: &str, icon: &str| MenuItem {
            label: label.into(),
            icon: icon.into(),
            actions: vec![],
        };
        let items = [
            item("Remove", "DELETE"),
            item("Block", "BLOCK"),
            item("Put user in timeout", "HOURGLASS"),
            item("Hide user on this channel", "REMOVE_CIRCLE"),
        ];
        assert_eq!(
            find_punish(&items, false).unwrap().label,
            "Put user in timeout"
        );
        assert_eq!(
            find_punish(&items, true).unwrap().label,
            "Hide user on this channel"
        );
        let undo = [item("Unhide user on this channel", "")];
        assert!(find_punish(&undo, true).is_none());
        assert!(
            find_punish(&items[..2], true).is_none(),
            "Block is not a ban"
        );
    }

    #[test]
    fn anonymous_empty_menu_has_no_items() {
        let v = json!({ "liveChatItemContextMenuSupportedRenderers": { "menuRenderer": { "openImmediately": true } } });
        assert!(parse_menu(&v).is_empty());
    }
}
