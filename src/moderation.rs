//! Moderator actions, run only when the user clicks a button. Nothing here
//! is ever triggered by a filter hit.
//!
//! Twitch login uses the OAuth "device code" flow: the app shows a short code,
//! the user approves it on twitch.tv/activate, and the app gets a token. No
//! client secret is needed, so this works in a desktop app you hand to others.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

const SCOPES: &str = "moderator:manage:chat_messages moderator:manage:banned_users";
const BAN_SCOPE: &str = "moderator:manage:banned_users";
const RELOGIN_FOR_BANS: &str =
    "Your Twitch login doesn't allow timeouts/bans yet. Log out and log in again in Settings.";
const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const HELIX: &str = "https://api.twitch.tv/helix";

/// Saved Twitch login. Kept in the app's data folder, never shown in the UI.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct TwitchAuth {
    pub access_token: String,
    pub refresh_token: String,
    /// Moderator's own user id and login (from token validation).
    pub user_id: String,
    pub login: String,
    /// Permissions granted at login; older logins lack the ban scope.
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl TwitchAuth {
    pub fn can_ban(&self) -> bool {
        self.scopes.iter().any(|s| s == BAN_SCOPE)
    }
}

/// Results from background work, delivered to the UI thread.
pub enum ModEvent {
    /// Device flow started: show this code and URL to the user.
    LoginCode {
        user_code: String,
        verify_url: String,
    },
    LoggedIn(TwitchAuth),
    LoginFailed(String),
    /// A delete finished. `key` identifies the message (stream id, message id).
    DeleteDone {
        key: (u64, String),
        result: Result<(), String>,
    },
    /// A Twitch timeout/ban finished: short summary, or why it failed.
    ActionDone {
        key: (u64, String),
        result: Result<String, String>,
    },
    /// The refresh token was used; persist the new pair.
    TokensRefreshed(TwitchAuth),
    /// Pasted YouTube cookie checked: the login, or why it failed.
    YtLogin(Result<crate::yt_mod::YoutubeAuth, String>),
    /// Google rotated cookies during a request; save the new set.
    YtCookiesUpdated(String),
    YtMenu {
        key: (u64, String),
        result: Result<Vec<crate::yt_mod::MenuItem>, String>,
    },
    YtActionDone {
        key: (u64, String),
        is_delete: bool,
        result: Result<String, String>,
    },
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn read_json(resp: &mut ureq::http::Response<ureq::Body>) -> Value {
    resp.body_mut().read_json::<Value>().unwrap_or(Value::Null)
}

/// Starts the device-code login on a background thread.
pub fn start_login(client_id: String, tx: Sender<ModEvent>, wake: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let result = device_login(&client_id, &tx, &wake);
        let ev = match result {
            Ok(auth) => ModEvent::LoggedIn(auth),
            Err(e) => ModEvent::LoginFailed(e),
        };
        let _ = tx.send(ev);
        wake();
    });
}

fn device_login(
    client_id: &str,
    tx: &Sender<ModEvent>,
    wake: &impl Fn(),
) -> Result<TwitchAuth, String> {
    if client_id.trim().is_empty() {
        return Err("Enter your Twitch Client ID in Settings first".into());
    }
    let http = agent();
    let mut resp = http
        .post(DEVICE_URL)
        .send_form([("client_id", client_id), ("scopes", SCOPES)])
        .map_err(|e| format!("Network error: {e}"))?;
    let status = resp.status().as_u16();
    let v = read_json(&mut resp);
    if status >= 400 {
        if v["message"].as_str() == Some("invalid client") {
            return Err("Twitch doesn't recognise that Client ID. Check it, and that the app's Client Type is Public.".into());
        }
        return Err(twitch_error(status, &v));
    }
    let device_code = v["device_code"]
        .as_str()
        .ok_or("Twitch didn't return a device code")?;
    let user_code = v["user_code"].as_str().unwrap_or_default().to_string();
    let verify_url = v["verification_uri"]
        .as_str()
        .unwrap_or("https://www.twitch.tv/activate")
        .to_string();
    let mut interval = Duration::from_secs(v["interval"].as_u64().unwrap_or(5));
    let expires = Instant::now() + Duration::from_secs(v["expires_in"].as_u64().unwrap_or(1800));
    let _ = tx.send(ModEvent::LoginCode {
        user_code,
        verify_url,
    });
    wake();

    loop {
        std::thread::sleep(interval);
        if Instant::now() > expires {
            return Err("Login code expired. Click Log in to try again.".into());
        }
        let mut resp = http
            .post(TOKEN_URL)
            .send_form([
                ("client_id", client_id),
                ("scopes", SCOPES),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .map_err(|e| format!("Network error: {e}"))?;
        let status = resp.status().as_u16();
        let v = read_json(&mut resp);
        if status < 400 {
            let access = v["access_token"].as_str().unwrap_or_default().to_string();
            let refresh = v["refresh_token"].as_str().unwrap_or_default().to_string();
            return validate(&http, access, refresh);
        }
        match v["message"].as_str().unwrap_or_default() {
            "authorization_pending" => continue,
            "slow_down" => interval += Duration::from_secs(5),
            "access_denied" => return Err("Login was declined on Twitch".into()),
            "expired_token" | "invalid device code" => {
                return Err("Login code expired. Click Log in to try again.".into());
            }
            _ => return Err(twitch_error(status, &v)),
        }
    }
}

/// Looks up who the token belongs to.
fn validate(http: &ureq::Agent, access: String, refresh: String) -> Result<TwitchAuth, String> {
    let mut resp = http
        .get(VALIDATE_URL)
        .header("Authorization", &format!("OAuth {access}"))
        .call()
        .map_err(|e| format!("Network error: {e}"))?;
    let status = resp.status().as_u16();
    let v = read_json(&mut resp);
    if status >= 400 {
        return Err(twitch_error(status, &v));
    }
    Ok(TwitchAuth {
        access_token: access,
        refresh_token: refresh,
        user_id: v["user_id"].as_str().unwrap_or_default().to_string(),
        login: v["login"].as_str().unwrap_or_default().to_string(),
        scopes: v["scopes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Deletes one Twitch chat message on a background thread. Refreshes the
/// token once if Twitch says it expired.
pub fn delete_twitch_message(
    client_id: String,
    auth: TwitchAuth,
    room_id: String,
    message_id: String,
    key: (u64, String),
    tx: Sender<ModEvent>,
    wake: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let http = agent();
        let mut auth = auth;
        let mut result = helix_delete(&http, &client_id, &auth, &room_id, &message_id);
        if matches!(&result, Err(HelixError::Unauthorized)) {
            match refresh(&http, &client_id, &auth) {
                Ok(new_auth) => {
                    auth = new_auth;
                    let _ = tx.send(ModEvent::TokensRefreshed(auth.clone()));
                    result = helix_delete(&http, &client_id, &auth, &room_id, &message_id);
                }
                Err(e) => result = Err(HelixError::Other(e)),
            }
        }
        let result = result.map_err(|e| match e {
            HelixError::Unauthorized => {
                "Twitch login expired. Log in again in Settings.".to_string()
            }
            HelixError::Other(s) => s,
        });
        let _ = tx.send(ModEvent::DeleteDone { key, result });
        wake();
    });
}

/// Times out (`duration` seconds) or bans (`None`) a Twitch user, on a
/// background thread.
#[allow(clippy::too_many_arguments)]
pub fn twitch_ban(
    client_id: String,
    auth: TwitchAuth,
    room_id: String,
    user_id: String,
    duration: Option<u32>,
    reason: String,
    key: (u64, String),
    tx: Sender<ModEvent>,
    wake: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let http = agent();
        let mut auth = auth;
        let call =
            |a: &TwitchAuth| helix_ban(&http, &client_id, a, &room_id, &user_id, duration, &reason);
        let mut result = call(&auth);
        if matches!(&result, Err(HelixError::Unauthorized)) {
            match refresh(&http, &client_id, &auth) {
                Ok(new_auth) => {
                    auth = new_auth;
                    let _ = tx.send(ModEvent::TokensRefreshed(auth.clone()));
                    result = call(&auth);
                }
                Err(e) => result = Err(HelixError::Other(e)),
            }
        }
        let result = result
            .map(|()| match duration {
                Some(s) => format!("timed out {}", human_duration(s)),
                None => "banned".to_string(),
            })
            .map_err(|e| match e {
                HelixError::Unauthorized => {
                    "Twitch login expired. Log in again in Settings.".to_string()
                }
                HelixError::Other(s) => s,
            });
        let _ = tx.send(ModEvent::ActionDone { key, result });
        wake();
    });
}

/// "10 min", "1 hour", "2 days".
pub fn human_duration(secs: u32) -> String {
    let (n, unit) = if secs >= 604_800 && secs.is_multiple_of(604_800) {
        (secs / 604_800, "week")
    } else if secs >= 86_400 && secs.is_multiple_of(86_400) {
        (secs / 86_400, "day")
    } else if secs >= 3600 && secs.is_multiple_of(3600) {
        (secs / 3600, "hour")
    } else if secs >= 60 && secs.is_multiple_of(60) {
        return format!("{} min", secs / 60);
    } else {
        return format!("{secs} sec");
    };
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

fn helix_ban(
    http: &ureq::Agent,
    client_id: &str,
    auth: &TwitchAuth,
    room_id: &str,
    user_id: &str,
    duration: Option<u32>,
    reason: &str,
) -> Result<(), HelixError> {
    if user_id.is_empty() {
        return Err(HelixError::Other(
            "Twitch didn't send this user's id".into(),
        ));
    }
    let url = format!(
        "{HELIX}/moderation/bans?broadcaster_id={}&moderator_id={}",
        enc(room_id),
        enc(&auth.user_id)
    );
    let mut data = json!({ "user_id": user_id });
    if let Some(d) = duration {
        data["duration"] = json!(d);
    }
    if !reason.trim().is_empty() {
        data["reason"] = json!(reason.trim());
    }
    let mut resp = http
        .post(&url)
        .header("Authorization", &format!("Bearer {}", auth.access_token))
        .header("Client-Id", client_id)
        .send_json(json!({ "data": data }))
        .map_err(|e| HelixError::Other(format!("Network error: {e}")))?;
    let status = resp.status().as_u16();
    if status == 200 {
        return Ok(());
    }
    let v = read_json(&mut resp);
    let msg = v["message"].as_str().unwrap_or_default().to_lowercase();
    Err(match status {
        401 if msg.contains("scope") => HelixError::Other(RELOGIN_FOR_BANS.into()),
        401 => HelixError::Unauthorized,
        403 => HelixError::Other(format!(
            "{} isn't a moderator in this channel",
            if auth.login.is_empty() {
                "Your account"
            } else {
                &auth.login
            }
        )),
        400 if msg.contains("already banned") => {
            HelixError::Other("That user is already banned".into())
        }
        400 if msg.contains("may not be banned")
            || msg.contains("broadcaster")
            || msg.contains("moderator") =>
        {
            HelixError::Other("Can't timeout or ban the broadcaster or another moderator".into())
        }
        409 => HelixError::Other("Someone else is changing this user's ban; try again".into()),
        429 => HelixError::Other("Twitch rate limit hit; wait a moment and try again".into()),
        _ => HelixError::Other(twitch_error(status, &v)),
    })
}

enum HelixError {
    Unauthorized,
    Other(String),
}

fn helix_delete(
    http: &ureq::Agent,
    client_id: &str,
    auth: &TwitchAuth,
    room_id: &str,
    message_id: &str,
) -> Result<(), HelixError> {
    let url = format!(
        "{HELIX}/moderation/chat?broadcaster_id={}&moderator_id={}&message_id={}",
        enc(room_id),
        enc(&auth.user_id),
        enc(message_id)
    );
    let mut resp = http
        .delete(&url)
        .header("Authorization", &format!("Bearer {}", auth.access_token))
        .header("Client-Id", client_id)
        .call()
        .map_err(|e| HelixError::Other(format!("Network error: {e}")))?;
    let status = resp.status().as_u16();
    if status == 204 {
        return Ok(());
    }
    let v = read_json(&mut resp);
    Err(match status {
        401 => HelixError::Unauthorized,
        403 => HelixError::Other(format!(
            "{} isn't a moderator in this channel",
            if auth.login.is_empty() {
                "Your account"
            } else {
                &auth.login
            }
        )),
        404 => {
            HelixError::Other("Message not found (already deleted, or older than 6 hours)".into())
        }
        400 => HelixError::Other(match v["message"].as_str() {
            Some(m) if m.contains("broadcaster") || m.contains("moderator") => {
                "Can't delete messages from the broadcaster or other moderators".into()
            }
            _ => twitch_error(status, &v),
        }),
        _ => HelixError::Other(twitch_error(status, &v)),
    })
}

fn refresh(http: &ureq::Agent, client_id: &str, auth: &TwitchAuth) -> Result<TwitchAuth, String> {
    if auth.refresh_token.is_empty() {
        return Err("Twitch login expired. Log in again in Settings.".into());
    }
    let mut resp = http
        .post(TOKEN_URL)
        .send_form([
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", auth.refresh_token.as_str()),
        ])
        .map_err(|e| format!("Network error: {e}"))?;
    let status = resp.status().as_u16();
    let v = read_json(&mut resp);
    if status >= 400 {
        return Err("Twitch login expired. Log in again in Settings.".into());
    }
    Ok(TwitchAuth {
        access_token: v["access_token"].as_str().unwrap_or_default().to_string(),
        refresh_token: v["refresh_token"]
            .as_str()
            .unwrap_or(&auth.refresh_token)
            .to_string(),
        user_id: auth.user_id.clone(),
        login: auth.login.clone(),
        scopes: auth.scopes.clone(),
    })
}

fn twitch_error(status: u16, v: &Value) -> String {
    let msg = v["message"].as_str().unwrap_or_default();
    if msg.is_empty() {
        format!("Twitch error {status}")
    } else {
        format!("Twitch error {status}: {msg}")
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
    fn durations_read_naturally() {
        let d: Vec<_> = [45, 60, 600, 3600, 7200, 86_400, 604_800, 90]
            .into_iter()
            .map(human_duration)
            .collect();
        assert_eq!(
            d,
            [
                "45 sec", "1 min", "10 min", "1 hour", "2 hours", "1 day", "1 week", "90 sec"
            ]
        );
    }

    #[test]
    fn old_logins_cannot_ban() {
        let old: TwitchAuth = serde_json::from_str(
            r#"{"access_token":"a","refresh_token":"r","user_id":"1","login":"me"}"#,
        )
        .unwrap();
        assert!(!old.can_ban());
        let new = TwitchAuth {
            scopes: vec![BAN_SCOPE.into()],
            ..old
        };
        assert!(new.can_ban());
    }
}
