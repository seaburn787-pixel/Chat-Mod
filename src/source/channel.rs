//! Finds every live and upcoming stream on a YouTube channel by reading its
//! "Live" tab. Upcoming streams have an open waiting-room chat, so they count too.

use super::Http;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct ChannelStream {
    pub video_id: String,
    pub title: String,
    pub upcoming: bool,
}

pub struct ChannelStreams {
    pub channel_name: String,
    pub streams: Vec<ChannelStream>,
}

/// `target` is "@handle", "UC…" id, or any youtube.com channel URL.
pub(super) fn list(http: &Http, target: &str) -> Result<ChannelStreams, String> {
    let html = http
        .get_text(&format!("{}/streams", channel_base(target)))
        .map_err(|e| {
            if e.contains("HTTP 404") {
                format!("channel {target} not found")
            } else {
                e
            }
        })?;
    let data = super::innertube::extract_json_after(&html, "ytInitialData")
        .ok_or_else(|| format!("Couldn't read the channel page for {target}"))?;
    let channel_name = data["metadata"]["channelMetadataRenderer"]["title"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if channel_name.is_empty() {
        return Err(format!("{target} doesn't look like a YouTube channel"));
    }
    Ok(ChannelStreams {
        channel_name,
        streams: parse(&data),
    })
}

pub fn channel_base(target: &str) -> String {
    let t = target.trim().trim_end_matches('/');
    if t.starts_with("http") {
        // Drop any tab suffix the user pasted (…/@name/streams, …/live, …/videos).
        let mut base = t.to_string();
        for tab in ["/streams", "/live", "/videos", "/featured", "/shorts"] {
            if let Some(stripped) = base.strip_suffix(tab) {
                base = stripped.to_string();
            }
        }
        base
    } else if t.starts_with("UC") && t.len() == 24 {
        format!("https://www.youtube.com/channel/{t}")
    } else {
        format!("https://www.youtube.com/@{}", t.trim_start_matches('@'))
    }
}

/// Live streams first, then upcoming, in the order the channel lists them.
fn parse(data: &Value) -> Vec<ChannelStream> {
    let mut found = Vec::new();
    collect(data, &mut found);
    let mut out: Vec<ChannelStream> = Vec::new();
    for s in found {
        if !out.iter().any(|o| o.video_id == s.video_id) {
            out.push(s);
        }
    }
    out.sort_by_key(|s| s.upcoming);
    out
}

fn collect(v: &Value, out: &mut Vec<ChannelStream>) {
    match v {
        Value::Object(map) => {
            if let Some(lockup) = map.get("lockupViewModel") {
                if let Some(s) = from_lockup(lockup) {
                    out.push(s);
                }
                return;
            }
            // Older page layout, still served to some clients.
            if let Some(r) = map.get("videoRenderer") {
                if let Some(s) = from_video_renderer(r) {
                    out.push(s);
                }
                return;
            }
            map.values().for_each(|x| collect(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| collect(x, out)),
        _ => {}
    }
}

fn from_lockup(l: &Value) -> Option<ChannelStream> {
    let video_id = l["contentId"].as_str()?.to_string();
    let mut badges = Vec::new();
    find_all(l, "thumbnailBadgeViewModel", &mut badges);
    let live = badges
        .iter()
        .any(|b| b["badgeStyle"].as_str() == Some("THUMBNAIL_OVERLAY_BADGE_STYLE_LIVE"));
    // "Upcoming" text is localized, but scheduled streams always say when they start.
    let upcoming = !live
        && (badges
            .iter()
            .any(|b| b["text"].as_str() == Some("Upcoming"))
            || has_text_starting(l, "Scheduled for"));
    if !live && !upcoming {
        return None;
    }
    let title = l["metadata"]["lockupMetadataViewModel"]["title"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Some(ChannelStream {
        video_id,
        title,
        upcoming,
    })
}

fn from_video_renderer(r: &Value) -> Option<ChannelStream> {
    let video_id = r["videoId"].as_str()?.to_string();
    let upcoming = r.get("upcomingEventData").is_some();
    let live =
        r["badges"].as_array().into_iter().flatten().any(|b| {
            b["metadataBadgeRenderer"]["style"].as_str() == Some("BADGE_STYLE_TYPE_LIVE_NOW")
        });
    if !live && !upcoming {
        return None;
    }
    let title = r["title"]["runs"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Some(ChannelStream {
        video_id,
        title,
        upcoming,
    })
}

fn find_all<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(map) => {
            for (k, x) in map {
                if k == key {
                    out.push(x);
                }
                find_all(x, key, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| find_all(x, key, out)),
        _ => {}
    }
}

fn has_text_starting(v: &Value, prefix: &str) -> bool {
    match v {
        Value::String(s) => s.starts_with(prefix),
        Value::Object(map) => map.values().any(|x| has_text_starting(x, prefix)),
        Value::Array(a) => a.iter().any(|x| has_text_starting(x, prefix)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lockup(id: &str, title: &str, badge_text: &str, style: &str, extra: &str) -> Value {
        json!({ "richItemRenderer": { "content": { "lockupViewModel": {
            "contentId": id,
            "contentImage": { "thumbnailViewModel": { "overlays": [ { "thumbnailBottomOverlayViewModel": {
                "badges": [ { "thumbnailBadgeViewModel": { "text": badge_text, "badgeStyle": style } } ] } } ] } },
            "metadata": { "lockupMetadataViewModel": {
                "title": { "content": title },
                "metadata": { "contentMetadataViewModel": { "metadataRows": [
                    { "metadataParts": [ { "text": { "content": extra } } ] } ] } } } }
        } } } })
    }

    #[test]
    fn picks_live_and_upcoming_only() {
        let page = json!({ "contents": [
            lockup("upcoming001", "Tomorrow", "Upcoming", "THUMBNAIL_OVERLAY_BADGE_STYLE_DEFAULT", "Scheduled for 10/8/26"),
            lockup("live0000001", "Now", "LIVE", "THUMBNAIL_OVERLAY_BADGE_STYLE_LIVE", "4.4K watching"),
            lockup("vod00000001", "Old", "2:08:56", "THUMBNAIL_OVERLAY_BADGE_STYLE_DEFAULT", "Streamed 31 min ago"),
            lockup("live0000001", "Now (dup)", "LIVE", "THUMBNAIL_OVERLAY_BADGE_STYLE_LIVE", ""),
        ] });
        let got = parse(&page);
        assert_eq!(
            got,
            vec![
                ChannelStream {
                    video_id: "live0000001".into(),
                    title: "Now".into(),
                    upcoming: false
                },
                ChannelStream {
                    video_id: "upcoming001".into(),
                    title: "Tomorrow".into(),
                    upcoming: true
                },
            ]
        );
    }

    #[test]
    fn localized_upcoming_still_found_by_schedule_text() {
        let page = lockup(
            "upcoming002",
            "x",
            "Próximamente",
            "THUMBNAIL_OVERLAY_BADGE_STYLE_DEFAULT",
            "Scheduled for 10/8/26",
        );
        assert_eq!(parse(&page).len(), 1);
    }

    #[test]
    fn channel_base_forms() {
        assert_eq!(channel_base("@SkyNews"), "https://www.youtube.com/@SkyNews");
        assert_eq!(channel_base("SkyNews"), "https://www.youtube.com/@SkyNews");
        assert_eq!(
            channel_base("https://www.youtube.com/@SkyNews/streams"),
            "https://www.youtube.com/@SkyNews"
        );
        assert_eq!(
            channel_base("UCoMdktPbSTixAyNGwb-UYkQ"),
            "https://www.youtube.com/channel/UCoMdktPbSTixAyNGwb-UYkQ"
        );
    }
}
