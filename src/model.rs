#[derive(Clone, Debug, PartialEq)]
pub struct FlagHit {
    pub list: String,
    pub rule: String,
}

/// A piece of a chat message as the platform rendered it.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    /// Emoji or channel emote. `alt` is the :shortcut: (or the emoji itself).
    Emote {
        url: String,
        alt: String,
    },
}

#[derive(Clone, Debug)]
pub struct ChatMsg {
    /// Which stream (merged chat) this came from.
    pub source: u64,
    pub id: String,
    pub author: String,
    pub channel_id: String,
    /// Author profile link when the platform isn't YouTube.
    pub profile_url: Option<String>,
    /// Plain text used for filtering and copying; emotes appear as their alt text.
    pub text: String,
    pub parts: Vec<Part>,
    pub time: String,
    pub is_owner: bool,
    pub is_mod: bool,
    pub is_member: bool,
    pub is_verified: bool,
    pub superchat: Option<String>,
    pub deleted: bool,
    pub hits: Vec<FlagHit>,
    /// The author's name contains a word from a list; shown highlighted.
    pub name_flagged: bool,
    /// Twitch only: the channel's user id, needed to moderate the message.
    pub room_id: Option<String>,
    /// YouTube only: params for the message's ⋮ menu.
    pub yt_menu: Option<String>,
    /// Set while a delete request is in flight / after it failed.
    pub mod_status: ModStatus,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum ModStatus {
    #[default]
    None,
    Deleting,
    /// A timeout/ban is in flight: "timing out…".
    Busy(String),
    /// Last action succeeded: "timed out 10 min".
    Info(String),
    Failed(String),
}

impl ChatMsg {
    pub fn is_flagged(&self) -> bool {
        !self.hits.is_empty()
    }

    pub fn channel_url(&self) -> Option<String> {
        if let Some(url) = &self.profile_url {
            return Some(url.clone());
        }
        (!self.channel_id.is_empty())
            .then(|| format!("https://www.youtube.com/channel/{}", self.channel_id))
    }
}

// Messages dominate the stream of events, so boxing them would only add allocations.
#[allow(clippy::large_enum_variant)]
pub enum SourceEvent {
    Status(String),
    /// Stream resolved: video id and channel name for the stream's label.
    Resolved {
        video_id: String,
        title: String,
    },
    Message(ChatMsg),
    Deleted(String),
    /// Author was banned / timed out: the platform removes all their messages.
    DeletedByAuthor(String),
    Ended {
        reason: String,
        error: bool,
    },
}

pub fn local_time_from_micros(us: i64) -> String {
    chrono::DateTime::from_timestamp_micros(us)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

pub fn local_time_from_rfc3339(s: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

pub fn now_time() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}
