use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// The endpoint youtube.com's own chat uses. No key or quota, but unofficial.
    Innertube,
    /// Official YouTube Data API v3. Needs an API key; limited by daily quota.
    Api,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Config {
    pub source: SourceKind,
    pub api_key: String,
    /// Floor on the API poll interval; each poll costs quota.
    pub api_min_poll_secs: u64,
    /// InnerTube poll interval. YouTube's page gets pushes; we approximate by polling.
    pub innertube_poll_ms: u64,
    /// Streams in the merged chat (URL, video ID, or @channel).
    pub streams: Vec<String>,
    /// Channels whose every live/upcoming stream is pulled in automatically.
    pub watched_channels: Vec<String>,
    /// How often watched channels are re-checked for new streams.
    pub channel_rescan_secs: u64,
    /// Render emoji and channel emotes as images.
    pub show_emotes: bool,
    pub sound_enabled: bool,
    /// Optional .wav/.oga to play on a flag; empty = system sound.
    pub sound_file: String,
    pub ignore_mods: bool,
    /// Flag words spelled out a letter or two per message.
    pub catch_spelled: bool,
    /// Flag one person sending the same message `repeat_count` times within
    /// `repeat_window_secs`.
    pub catch_repeats: bool,
    pub repeat_count: usize,
    pub repeat_window_secs: u64,
    /// Flag `raid_count` different people sending the same message within
    /// `raid_window_secs` (copy-paste raids).
    pub catch_raids: bool,
    pub raid_count: usize,
    pub raid_window_secs: u64,
    pub max_messages: usize,
    pub disabled_lists: Vec<String>,
    /// From dev.twitch.tv; identifies this app when logging in to moderate.
    pub twitch_client_id: String,
    /// Last Twitch timeout length picked, in seconds.
    pub twitch_timeout_secs: u32,
    /// 🗑 ⏱ 🔨 buttons at the start of every chat message.
    pub show_mod_buttons: bool,
    /// Lists whose words are also looked for inside usernames.
    pub name_check_lists: Vec<String>,
    pub log_flagged: bool,
}

impl Config {
    pub fn repeat_settings(&self) -> crate::repeats::Settings {
        use crate::repeats::Rule;
        use std::time::Duration;
        crate::repeats::Settings {
            repeats: self.catch_repeats.then(|| Rule {
                count: self.repeat_count.clamp(2, 20),
                window: Duration::from_secs(self.repeat_window_secs.clamp(5, 600)),
            }),
            raids: self.catch_raids.then(|| Rule {
                count: self.raid_count.clamp(2, 50),
                window: Duration::from_secs(self.raid_window_secs.clamp(5, 600)),
            }),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            source: SourceKind::Innertube,
            api_key: String::new(),
            api_min_poll_secs: 5,
            innertube_poll_ms: 1000,
            streams: Vec::new(),
            watched_channels: Vec::new(),
            channel_rescan_secs: 120,
            show_emotes: true,
            sound_enabled: true,
            sound_file: String::new(),
            ignore_mods: true,
            catch_spelled: true,
            catch_repeats: true,
            repeat_count: 3,
            repeat_window_secs: 60,
            catch_raids: true,
            raid_count: 4,
            raid_window_secs: 30,
            max_messages: 1500,
            disabled_lists: Vec::new(),
            twitch_client_id: String::new(),
            twitch_timeout_secs: 600,
            show_mod_buttons: true,
            name_check_lists: vec!["profanity".into()],
            log_flagged: true,
        }
    }
}

pub struct Paths {
    pub dir: PathBuf,
    pub config_file: PathBuf,
    pub lists_dir: PathBuf,
    pub log_file: PathBuf,
    /// Twitch login tokens, kept out of config.toml so that file is safe to share.
    pub twitch_auth_file: PathBuf,
    /// YouTube login cookies. Effectively a Google login: never share this file.
    pub youtube_auth_file: PathBuf,
}

impl Paths {
    pub fn init() -> Self {
        let dir = directories::ProjectDirs::from("", "", "chat-mod")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("chat-mod-data"));
        let paths = Self {
            config_file: dir.join("config.toml"),
            lists_dir: dir.join("lists"),
            log_file: dir.join("flagged.log"),
            twitch_auth_file: dir.join("twitch_login.json"),
            youtube_auth_file: dir.join("youtube_login.json"),
            dir,
        };
        let _ = fs::create_dir_all(&paths.lists_dir);
        paths.seed_default_lists();
        let allow = paths.list_path(crate::filter::NAME_ALLOW_LIST);
        if !allow.exists() {
            let _ = fs::write(allow, DEFAULT_NAMES_OK);
        }
        paths
    }

    fn seed_default_lists(&self) {
        let empty = fs::read_dir(&self.lists_dir)
            .map(|mut rd| rd.next().is_none())
            .unwrap_or(true);
        if !empty {
            return;
        }
        let _ = fs::write(self.lists_dir.join("spam.txt"), DEFAULT_SPAM);
        let _ = fs::write(self.lists_dir.join("profanity.txt"), DEFAULT_PROFANITY);
        let _ = fs::write(self.lists_dir.join("watch_users.txt"), DEFAULT_USERS);
    }

    pub fn load_config(&self) -> Config {
        fs::read_to_string(&self.config_file)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn load_twitch_auth(&self) -> Option<crate::moderation::TwitchAuth> {
        fs::read_to_string(&self.twitch_auth_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn save_twitch_auth(&self, auth: Option<&crate::moderation::TwitchAuth>) {
        match auth {
            Some(a) => {
                if let Ok(s) = serde_json::to_string_pretty(a) {
                    let _ = fs::write(&self.twitch_auth_file, s);
                }
            }
            None => {
                let _ = fs::remove_file(&self.twitch_auth_file);
            }
        }
    }

    pub fn load_youtube_auth(&self) -> Option<crate::yt_mod::YoutubeAuth> {
        fs::read_to_string(&self.youtube_auth_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn save_youtube_auth(&self, auth: Option<&crate::yt_mod::YoutubeAuth>) {
        match auth {
            Some(a) => {
                if let Ok(s) = serde_json::to_string_pretty(a) {
                    let _ = fs::write(&self.youtube_auth_file, s);
                }
            }
            None => {
                let _ = fs::remove_file(&self.youtube_auth_file);
            }
        }
    }

    pub fn save_config(&self, cfg: &Config) {
        if let Ok(s) = toml::to_string_pretty(cfg) {
            let _ = fs::write(&self.config_file, s);
        }
    }

    pub fn list_path(&self, name: &str) -> PathBuf {
        self.lists_dir.join(format!("{name}.txt"))
    }
}

pub const LIST_HEADER: &str = "\
# One rule per line. Lines starting with # are comments.
#   word or phrase  -> whole word, any case; also catches leetspeak (sc4m),
#                      spacing (s c a m), lookalike/accented letters, stretching (scaaam)
#   word*  *word  *word*  -> starts with / ends with / contains
#   re:<regex>      -> regular expression (case-insensitive)
#   user:<name>     -> exact author name, or user:UC... channel id
#   name:<text>     -> anyone whose name CONTAINS this (catches JohnnyV2, xX_j0hnny_Xx)
";

const DEFAULT_SPAM: &str = concat!(
    "# Spam and scam patterns.\n",
    "# One rule per line. Lines starting with # are comments.\n",
    "#   word or phrase  -> whole word, any case; also catches leetspeak (sc4m),\n",
    "#                      spacing (s c a m), lookalike/accented letters, stretching (scaaam)\n",
    "#   word*  *word  *word*  -> starts with / ends with / contains\n",
    "#   re:<regex>      -> regular expression (case-insensitive)\n",
    "#   user:<name>     -> exact author name, or user:UC... channel id\n",
    "\n",
    "free robux\n",
    "free vbucks\n",
    "crypto giveaway\n",
    "double your money\n",
    "whats*app\n",
    "telegram\n",
    "re:(t\\.me|wa\\.me|bit\\.ly|tinyurl\\.com)/\n",
    "re:\\+\\d[\\d\\s-]{9,}\n",
);

const DEFAULT_PROFANITY: &str = concat!(
    "# Add the words you want flagged, one per line.\n",
    "# Leetspeak / spacing / stretched-letter variants are caught automatically,\n",
    "# so you only need the plain spelling. Use word* for all endings.\n",
);

const DEFAULT_USERS: &str = concat!(
    "# Authors to always flag.\n",
    "# user:SomeName\n",
    "# user:UCxxxxxxxxxxxxxxxxxxxxxx\n",
    "# name:johnny   <- anyone whose name contains this, leetspeak and all\n",
);

const DEFAULT_NAMES_OK: &str = concat!(
    "# Usernames that should never be flagged for containing a list word,\n",
    "# e.g. Cassandra when \"ass\" is on a list. One name per line.\n",
    "# This doesn't affect name: or user: rules.\n",
);
