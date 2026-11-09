use crate::config::{Config, LIST_HEADER, Paths, SourceKind};
use crate::filter::FilterSet;
use crate::model::{ChatMsg, FlagHit, ModStatus, Part, SourceEvent};
use crate::moderation::{self, ModEvent, TwitchAuth};
use crate::repeats::RepeatTracker;
use crate::sound;
use crate::source::channel::ChannelStreams;
use crate::source::{self, SourceHandle, StartParams};
use crate::spelled::SpellTracker;
use crate::yt_mod::{self, MenuAction, MenuItem, YoutubeAuth};
use eframe::egui::{self, Color32, RichText};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

const FLAG_BG: Color32 = Color32::from_rgba_premultiplied(120, 30, 30, 140);
const FLAG_ACCENT: Color32 = Color32::from_rgb(255, 110, 110);
const OWNER_COLOR: Color32 = Color32::from_rgb(255, 214, 0);
const MOD_COLOR: Color32 = Color32::from_rgb(94, 132, 241);
const MEMBER_COLOR: Color32 = Color32::from_rgb(43, 166, 64);
const SUPERCHAT_COLOR: Color32 = Color32::from_rgb(230, 160, 40);
const DIM: Color32 = Color32::from_gray(130);
/// Behind usernames that contain a list word.
const NAME_FLAG_BG: Color32 = Color32::from_rgb(170, 40, 40);
const STOP_FILL: Color32 = Color32::from_rgb(160, 40, 48);
/// Collapses a burst of flags into one alert.
const SOUND_COOLDOWN: Duration = Duration::from_millis(1500);
const MAX_FLAGGED: usize = 500;
/// Emotes not drawn for this long are dropped from memory (re-downloaded if
/// they scroll back into view).
const EMOTE_KEEP: Duration = Duration::from_secs(30);
const EMOTE_SWEEP_EVERY: Duration = Duration::from_secs(10);
/// Height guess for chat rows not laid out yet; corrected once drawn.
const ROW_HEIGHT_GUESS: f32 = 22.0;
/// Emote height relative to the body font.
const EMOTE_SCALE: f32 = 1.5;
/// Per-stream tag colors in the merged chat, assigned in order.
const STREAM_COLORS: [Color32; 6] = [
    Color32::from_rgb(86, 182, 255),
    Color32::from_rgb(255, 160, 80),
    Color32::from_rgb(190, 130, 255),
    Color32::from_rgb(80, 210, 170),
    Color32::from_rgb(255, 120, 180),
    Color32::from_rgb(220, 210, 90),
];

struct FlaggedEntry {
    msg: ChatMsg,
    reviewed: bool,
    /// For a word spelled across messages: the pieces as typed, "x · y · z".
    spelled: Option<String>,
    /// Flagged for the username, not the message.
    name_only: bool,
}

#[derive(PartialEq)]
enum ConnState {
    Idle,
    Connecting,
    Live,
}

struct Stream {
    id: u64,
    target: String,
    /// Channel name once resolved, else the target as typed.
    label: String,
    video_id: String,
    color: Color32,
    handle: Option<SourceHandle>,
    conn: ConnState,
    status: String,
    status_is_error: bool,
    /// Set when a watched channel added this stream; it's removed automatically
    /// once the channel no longer lists it.
    from_channel: Option<String>,
    upcoming: bool,
    /// The streamer turned chat off; nothing to read, but not a failure.
    chat_disabled: bool,
}

impl Stream {
    fn new(id: u64, target: String, color: Color32) -> Self {
        Self {
            id,
            label: short_label(&target),
            target,
            video_id: String::new(),
            color,
            handle: None,
            conn: ConnState::Idle,
            status: "Not connected".into(),
            status_is_error: false,
            from_channel: None,
            upcoming: false,
            chat_disabled: false,
        }
    }

    fn stop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.stop();
        }
        self.conn = ConnState::Idle;
    }
}

fn short_label(target: &str) -> String {
    let t = target.trim().trim_end_matches('/');
    if let Some(at) = t.rfind('@') {
        return t[at..].split('/').next().unwrap_or(t).to_string();
    }
    source::parse_video_id(t).unwrap_or_else(|| t.to_string())
}

struct WatchedChannel {
    /// As typed: @handle, channel URL, or UC… id.
    target: String,
    name: String,
    scanning: bool,
    last_scan: Option<Instant>,
    error: Option<String>,
    live: usize,
    upcoming: usize,
    /// Streams the user removed by hand; rescans won't re-add them.
    dismissed: Vec<String>,
}

impl WatchedChannel {
    fn new(target: String) -> Self {
        Self {
            target,
            name: String::new(),
            scanning: false,
            last_scan: None,
            error: None,
            live: 0,
            upcoming: 0,
            dismissed: Vec::new(),
        }
    }
}

/// "Sky News · Mornings with Ridge and…" so streams from one channel are told apart.
fn stream_label(channel: &str, title: &str) -> String {
    const MAX: usize = 32;
    let short: String = title.chars().take(MAX).collect();
    let short = if title.chars().count() > MAX {
        format!("{}…", short.trim_end())
    } else {
        short
    };
    if channel.is_empty() {
        short
    } else {
        format!("{channel} · {short}")
    }
}

pub struct ModApp {
    paths: Paths,
    cfg: Config,
    saved_cfg: Config,
    filters: FilterSet,

    tx: Sender<(u64, SourceEvent)>,
    rx: Receiver<(u64, SourceEvent)>,
    streams: Vec<Stream>,
    next_stream_id: u64,
    new_target: String,

    channels: Vec<WatchedChannel>,
    discover_tx: Sender<(String, Result<ChannelStreams, String>)>,
    discover_rx: Receiver<(String, Result<ChannelStreams, String>)>,
    confirm_quit: bool,

    chat: VecDeque<ChatMsg>,
    flagged: VecDeque<FlaggedEntry>,
    total_seen: u64,
    auto_scroll: bool,
    show_reviewed: bool,
    last_sound: Option<Instant>,
    spell: SpellTracker,
    repeats: RepeatTracker,
    /// (stream, author id, name) already checked, so a name flags once per
    /// person and again only if they rename.
    names_checked: HashSet<(u64, String, String)>,
    /// Authors whose name matched; all their messages get the name highlight.
    flagged_names: HashSet<(u64, String)>,
    name_test: String,
    /// Measured height of each chat row, same order as `chat`; lets the chat
    /// lay out only the rows on screen.
    row_heights: VecDeque<f32>,
    /// Emote URI -> last time it was drawn.
    emote_seen: HashMap<String, Instant>,
    last_emote_sweep: Instant,

    twitch_auth: Option<TwitchAuth>,
    mod_tx: Sender<ModEvent>,
    mod_rx: Receiver<ModEvent>,
    /// Device-login code to show while the user approves on twitch.tv/activate.
    login_code: Option<(String, String)>,
    login_status: Option<String>,
    logging_in: bool,
    /// Message waiting for the user to confirm deletion: (stream, message id).
    confirm_delete: Option<(u64, String)>,

    yt_auth: Option<YoutubeAuth>,
    yt_cookie_input: String,
    yt_checking: bool,
    yt_status: Option<String>,
    /// The ⋮ actions window for one YouTube message.
    yt_menu: Option<YtMenuView>,
    /// A ⋮ action waiting for the user to confirm it.
    confirm_action: Option<PendingAction>,
    /// Timeout/ban dialog.
    punish: Option<PunishView>,

    show_settings: bool,
    show_lists: bool,
    editing_list: Option<(String, String)>,
    new_list_name: String,
    test_text: String,
    spell_test: String,
}

impl ModApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        egui_extras::install_image_loaders(&cc.egui_ctx);
        // Static images: drop the downloaded/decoded copy once it's on the GPU.
        cc.egui_ctx.options_mut(|o| o.reduce_texture_memory = true);
        let paths = Paths::init();
        let cfg = paths.load_config();
        let filters = FilterSet::load(&paths.lists_dir, &cfg.disabled_lists, &cfg.name_check_lists);
        let (tx, rx) = channel();
        let (discover_tx, discover_rx) = channel();
        let (mod_tx, mod_rx) = channel();
        let twitch_auth = paths.load_twitch_auth();
        let yt_auth = paths.load_youtube_auth();
        let channels = cfg
            .watched_channels
            .iter()
            .map(|t| WatchedChannel::new(t.clone()))
            .collect();
        let streams: Vec<Stream> = cfg
            .streams
            .iter()
            .enumerate()
            .map(|(i, t)| Stream::new(i as u64, t.clone(), STREAM_COLORS[i % STREAM_COLORS.len()]))
            .collect();
        Self {
            next_stream_id: streams.len() as u64,
            streams,
            new_target: String::new(),
            channels,
            discover_tx,
            discover_rx,
            confirm_quit: false,
            saved_cfg: cfg.clone(),
            cfg,
            filters,
            paths,
            tx,
            rx,
            chat: VecDeque::new(),
            flagged: VecDeque::new(),
            total_seen: 0,
            auto_scroll: true,
            show_reviewed: false,
            last_sound: None,
            spell: SpellTracker::default(),
            repeats: RepeatTracker::default(),
            names_checked: HashSet::new(),
            flagged_names: HashSet::new(),
            name_test: String::new(),
            row_heights: VecDeque::new(),
            emote_seen: HashMap::new(),
            last_emote_sweep: Instant::now(),
            twitch_auth,
            mod_tx,
            mod_rx,
            login_code: None,
            login_status: None,
            logging_in: false,
            confirm_delete: None,
            yt_auth,
            yt_cookie_input: String::new(),
            yt_checking: false,
            yt_status: None,
            yt_menu: None,
            confirm_action: None,
            punish: None,
            show_settings: false,
            show_lists: false,
            editing_list: None,
            new_list_name: String::new(),
            test_text: String::new(),
            spell_test: String::new(),
        }
    }

    fn stream_index(&self, id: u64) -> Option<usize> {
        self.streams.iter().position(|s| s.id == id)
    }

    fn any_connected(&self) -> bool {
        self.streams.iter().any(|s| s.conn != ConnState::Idle)
    }

    fn connect(&mut self, idx: usize, ctx: &egui::Context) {
        let params = StartParams {
            kind: self.cfg.source,
            target: self.streams[idx].target.clone(),
            api_key: self.cfg.api_key.clone(),
            api_min_poll_secs: self.cfg.api_min_poll_secs,
            innertube_poll_ms: self.cfg.innertube_poll_ms,
        };
        let st = &mut self.streams[idx];
        st.stop();
        match source::start(st.id, params, self.tx.clone(), ctx.clone()) {
            Ok(h) => {
                st.handle = Some(h);
                st.conn = ConnState::Connecting;
                st.status = "Connecting…".into();
                st.status_is_error = false;
            }
            Err(e) => {
                st.status = e;
                st.status_is_error = true;
            }
        }
    }

    fn connect_all(&mut self, ctx: &egui::Context) {
        for i in 0..self.streams.len() {
            if self.streams[i].conn == ConnState::Idle {
                self.connect(i, ctx);
            }
        }
    }

    fn disconnect_all(&mut self) {
        for st in &mut self.streams {
            if st.conn != ConnState::Idle {
                st.stop();
                st.status = "Disconnected".into();
                st.status_is_error = false;
            }
        }
    }

    fn add_stream(&mut self, ctx: &egui::Context) {
        let target = self.new_target.trim().to_string();
        if target.is_empty() || self.streams.iter().any(|s| s.target == target) {
            return;
        }
        if source::is_youtube_channel(&target) {
            self.new_target.clear();
            self.watch_channel(target, ctx);
            return;
        }
        let color = STREAM_COLORS[self.streams.len() % STREAM_COLORS.len()];
        self.streams
            .push(Stream::new(self.next_stream_id, target, color));
        self.next_stream_id += 1;
        self.new_target.clear();
        self.sync_stream_config();
        // Joining a merged chat that's already running: connect right away.
        if self.any_connected() {
            self.connect(self.streams.len() - 1, ctx);
        }
    }

    fn remove_stream(&mut self, idx: usize) {
        let mut st = self.streams.remove(idx);
        st.stop();
        if let Some(ch) = &st.from_channel
            && let Some(w) = self.channels.iter_mut().find(|w| w.target == *ch)
        {
            w.dismissed.push(st.video_id.clone());
        }
        self.sync_stream_config();
    }

    /// Channel-added streams aren't saved: they're rediscovered on the next scan.
    fn sync_stream_config(&mut self) {
        self.cfg.streams = self
            .streams
            .iter()
            .filter(|s| s.from_channel.is_none())
            .map(|s| s.target.clone())
            .collect();
        self.cfg.watched_channels = self.channels.iter().map(|w| w.target.clone()).collect();
    }

    fn watch_channel(&mut self, target: String, ctx: &egui::Context) {
        if self.channels.iter().any(|w| w.target == target) {
            return;
        }
        self.channels.push(WatchedChannel::new(target));
        self.sync_stream_config();
        self.scan_channels(ctx, true);
    }

    fn unwatch_channel(&mut self, idx: usize) {
        let w = self.channels.remove(idx);
        let mut i = 0;
        while i < self.streams.len() {
            if self.streams[i].from_channel.as_deref() == Some(w.target.as_str()) {
                self.streams.remove(i).stop();
            } else {
                i += 1;
            }
        }
        self.sync_stream_config();
    }

    /// Re-reads each watched channel's Live tab when it's due (or `force`).
    fn scan_channels(&mut self, ctx: &egui::Context, force: bool) {
        let every = Duration::from_secs(self.cfg.channel_rescan_secs.max(30));
        for w in &mut self.channels {
            let due = w.last_scan.is_none_or(|t| t.elapsed() >= every);
            if !w.scanning && (force || due) {
                w.scanning = true;
                w.last_scan = Some(Instant::now());
                source::discover_channel(w.target.clone(), ctx.clone(), self.discover_tx.clone());
            }
        }
    }

    /// Adds newly found streams, drops ones the channel no longer lists.
    fn on_discovered(
        &mut self,
        target: String,
        result: Result<ChannelStreams, String>,
        ctx: &egui::Context,
    ) {
        let Some(w) = self.channels.iter_mut().find(|w| w.target == target) else {
            return; // unwatched while the scan was running
        };
        w.scanning = false;
        let found = match result {
            Ok(r) => {
                w.name = r.channel_name;
                w.error = None;
                r.streams
            }
            Err(e) => {
                w.error = Some(e);
                return; // keep current streams; a page hiccup shouldn't drop them
            }
        };
        w.live = found.iter().filter(|s| !s.upcoming).count();
        w.upcoming = found.len() - w.live;
        let dismissed = w.dismissed.clone();
        let channel_name = w.name.clone();

        // Drop streams that ended or were taken down.
        let mut i = 0;
        while i < self.streams.len() {
            let st = &self.streams[i];
            let gone = st.from_channel.as_deref() == Some(target.as_str())
                && !found.iter().any(|f| f.video_id == st.video_id);
            if gone {
                self.streams.remove(i).stop();
            } else {
                i += 1;
            }
        }

        // Adding a channel should start its chats without also pressing Connect.
        for f in found {
            if dismissed.contains(&f.video_id) {
                continue;
            }
            if let Some(st) = self.streams.iter_mut().find(|s| s.video_id == f.video_id) {
                st.upcoming = f.upcoming; // waiting room went live
                continue;
            }
            let color = STREAM_COLORS[self.streams.len() % STREAM_COLORS.len()];
            let mut st = Stream::new(
                self.next_stream_id,
                format!("https://www.youtube.com/watch?v={}", f.video_id),
                color,
            );
            self.next_stream_id += 1;
            st.video_id = f.video_id;
            st.label = stream_label(&channel_name, &f.title);
            st.from_channel = Some(target.clone());
            st.upcoming = f.upcoming;
            self.streams.push(st);
            self.connect(self.streams.len() - 1, ctx);
        }
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.disconnect_all();
        self.paths.save_config(&self.cfg);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn drain_events(&mut self) {
        while let Ok((sid, ev)) = self.rx.try_recv() {
            // Events from a removed stream's thread that was still winding down.
            let Some(idx) = self.stream_index(sid) else {
                continue;
            };
            match ev {
                SourceEvent::Status(s) => {
                    let st = &mut self.streams[idx];
                    if s.starts_with("Connected") {
                        st.conn = ConnState::Live;
                    }
                    st.status = s;
                    st.status_is_error = false;
                }
                SourceEvent::Resolved { video_id, title } => {
                    let st = &mut self.streams[idx];
                    st.video_id = video_id;
                    // Channel streams already have a "Channel · title" label.
                    if !title.is_empty() && st.from_channel.is_none() {
                        st.label = title;
                    }
                }
                SourceEvent::Message(m) => self.on_message(m),
                SourceEvent::Deleted(id) => self.mark_deleted(|m| m.id == id),
                SourceEvent::DeletedByAuthor(ch) => {
                    self.mark_deleted(|m| m.source == sid && m.channel_id == ch)
                }
                SourceEvent::Ended { reason, error } => {
                    let st = &mut self.streams[idx];
                    st.handle = None;
                    st.conn = ConnState::Idle;
                    st.chat_disabled = reason.to_lowercase().contains("chat is disabled");
                    st.status_is_error = error && !st.chat_disabled;
                    st.status = reason;
                }
            }
        }
    }

    fn on_message(&mut self, mut m: ChatMsg) {
        if self
            .chat
            .iter()
            .rev()
            .take(400)
            .any(|c| c.id == m.id && c.source == m.source)
        {
            return; // InnerTube replays recent messages on reconnect
        }
        self.total_seen += 1;
        let exempt = m.is_owner || (self.cfg.ignore_mods && m.is_mod);
        let mut spelled = None;
        let mut name_hits = Vec::new();
        if !exempt {
            m.hits = self.filters.check(&m.author, &m.channel_id, &m.text);
            if self.cfg.catch_spelled {
                spelled = self.check_spelled(&mut m);
            }
            self.check_repeats(&mut m);
            name_hits = self.check_author_name(&mut m);
        }
        if m.is_flagged() {
            self.on_flag(&m, spelled, false);
        } else if !name_hits.is_empty() {
            // Flag the person once, on their first message.
            let mut shown = m.clone();
            shown.hits = name_hits;
            self.on_flag(&shown, None, true);
        }
        self.chat.push_back(m);
        self.row_heights.push_back(ROW_HEIGHT_GUESS);
        while self.chat.len() > self.cfg.max_messages.max(100) {
            self.chat.pop_front();
            self.row_heights.pop_front();
        }
    }

    /// Feeds the message to the spelled-out-word tracker. On a hit, the earlier
    /// fragments are marked too, and the pieces are returned for the Flagged panel.
    fn check_spelled(&mut self, m: &mut ChatMsg) -> Option<String> {
        let who = if m.channel_id.is_empty() {
            m.author.clone()
        } else {
            m.channel_id.clone()
        };
        let hit = self.spell.push(
            &self.filters,
            (m.source, who),
            &m.id,
            &m.text,
            Instant::now(),
        )?;
        for earlier in self
            .chat
            .iter_mut()
            .filter(|c| c.source == m.source && hit.ids.contains(&c.id))
        {
            earlier.hits.extend(hit.hits.iter().cloned());
        }
        m.hits.extend(hit.hits);
        Some(hit.combined)
    }

    /// Same message over and over from one person, or from many people at
    /// once. Every copy that counted is marked, not just the newest.
    fn check_repeats(&mut self, m: &mut ChatMsg) {
        let settings = self.cfg.repeat_settings();
        if settings == Default::default() {
            return;
        }
        // Compare the words only: emote-only and emote-heavy spam is left alone.
        let words: String = m
            .parts
            .iter()
            .map(|p| match p {
                Part::Text(t) => t.as_str(),
                Part::Emote { .. } => " ",
            })
            .collect();
        let who = if m.channel_id.is_empty() {
            &m.author
        } else {
            &m.channel_id
        };
        let found = self
            .repeats
            .push(&settings, m.source, who, &m.id, &words, Instant::now());
        for hit in found {
            for earlier in self
                .chat
                .iter_mut()
                .filter(|c| c.source == m.source && hit.ids.contains(&c.id))
            {
                earlier.hits.push(hit.hit.clone());
            }
            m.hits.push(hit.hit);
        }
    }

    /// Looks for list words inside the author's name, once per name. Returns
    /// the hits the first time; marks every message from them either way.
    fn check_author_name(&mut self, m: &mut ChatMsg) -> Vec<FlagHit> {
        let who = if m.channel_id.is_empty() {
            m.author.clone()
        } else {
            m.channel_id.clone()
        };
        let key = (m.source, who.clone(), m.author.clone());
        let first_time = self.names_checked.insert(key);
        let mut hits = Vec::new();
        if first_time {
            hits = self.filters.check_name(&m.author);
            if hits.is_empty() {
                self.flagged_names.remove(&(m.source, who.clone()));
            } else {
                self.flagged_names.insert((m.source, who.clone()));
            }
        }
        m.name_flagged = self.flagged_names.contains(&(m.source, who));
        hits
    }

    fn on_flag(&mut self, m: &ChatMsg, spelled: Option<String>, name_only: bool) {
        if self.cfg.sound_enabled
            && self
                .last_sound
                .is_none_or(|t| t.elapsed() >= SOUND_COOLDOWN)
        {
            sound::play(&self.cfg.sound_file);
            self.last_sound = Some(Instant::now());
        }
        if self.cfg.log_flagged {
            self.log_flag(m);
        }
        self.flagged.push_back(FlaggedEntry {
            msg: m.clone(),
            reviewed: false,
            spelled,
            name_only,
        });
        while self.flagged.len() > MAX_FLAGGED {
            self.flagged.pop_front();
        }
    }

    fn log_flag(&self, m: &ChatMsg) {
        let rules: Vec<String> = m
            .hits
            .iter()
            .map(|h| format!("{}:{}", h.list, h.rule))
            .collect();
        let video = self
            .stream_index(m.source)
            .map(|i| self.streams[i].video_id.as_str())
            .unwrap_or("");
        let line = format!(
            "{} [{video}] {} ({}) : {} | {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            m.author,
            m.channel_id,
            m.text.replace('\n', " "),
            rules.join(", ")
        );
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.paths.log_file)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn mark_deleted(&mut self, pred: impl Fn(&ChatMsg) -> bool) {
        for m in self.chat.iter_mut().filter(|m| pred(m)) {
            m.deleted = true;
        }
        for f in self.flagged.iter_mut().filter(|f| pred(&f.msg)) {
            f.msg.deleted = true;
            f.reviewed = true; // already handled in YouTube
        }
    }

    fn reload_filters(&mut self) {
        self.filters = FilterSet::load(
            &self.paths.lists_dir,
            &self.cfg.disabled_lists,
            &self.cfg.name_check_lists,
        );
        // Re-check names against the edited lists.
        self.names_checked.clear();
        self.flagged_names.clear();
    }

    fn unreviewed_count(&self) -> usize {
        self.flagged.iter().filter(|f| !f.reviewed).count()
    }

    fn persist_config(&mut self) {
        if self.cfg != self.saved_cfg {
            self.paths.save_config(&self.cfg);
            self.saved_cfg = self.cfg.clone();
        }
    }

    // ---------------- UI ----------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let connected = self.any_connected();
        ui.horizontal(|ui| {
            // Right-hand buttons first, so a narrow window squeezes the input box
            // instead of pushing Stop off the edge.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("⏻ Stop").color(Color32::WHITE).strong())
                            .fill(STOP_FILL),
                    )
                    .on_hover_text("Disconnect everything and close the program")
                    .clicked()
                {
                    self.confirm_quit = true;
                }
                if ui.button("Settings").clicked() {
                    self.show_settings = !self.show_settings;
                }
                if ui.button("Word lists").clicked() {
                    self.show_lists = !self.show_lists;
                }
                let (label, color) = if self.cfg.sound_enabled {
                    ("🔊 Sound on", MEMBER_COLOR)
                } else {
                    ("🔇 Sound off", FLAG_ACCENT)
                };
                if ui
                    .button(RichText::new(label).color(color))
                    .on_hover_text("Toggle flag sound")
                    .clicked()
                {
                    self.cfg.sound_enabled = !self.cfg.sound_enabled;
                    if self.cfg.sound_enabled {
                        sound::play(&self.cfg.sound_file);
                    }
                }
                ui.separator();

                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(!connected, |ui| {
                        egui::ComboBox::from_id_salt("source")
                            .selected_text(match self.cfg.source {
                                SourceKind::Innertube => "YouTube: InnerTube",
                                SourceKind::Api => "YouTube: API",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.cfg.source,
                                    SourceKind::Innertube,
                                    "InnerTube (no key)",
                                );
                                ui.selectable_value(
                                    &mut self.cfg.source,
                                    SourceKind::Api,
                                    "YouTube API (key + quota)",
                                );
                            })
                            .response
                            .on_hover_text(
                                "How YouTube streams are read. Twitch links are detected automatically.",
                            )
                            .on_disabled_hover_text(
                                "Disconnect all streams to change how YouTube is read",
                            );
                    });
                    // Leave room for Add + Connect/Disconnect after the box.
                    let input_width = (ui.available_width() - 190.0).clamp(140.0, 360.0);
                    let input = egui::TextEdit::singleline(&mut self.new_target)
                        .hint_text("YouTube @channel (all its streams), video URL, or twitch.tv/name")
                        .desired_width(input_width);
                    let resp = ui.add(input);
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.add_stream(&ctx);
                        resp.request_focus();
                    }
                    if ui.button("➕ Add").clicked() {
                        self.add_stream(&ctx);
                    }
                    if connected {
                        if ui.button("Disconnect all").clicked() {
                            self.disconnect_all();
                        }
                    } else if ui
                        .add_enabled(!self.streams.is_empty(), egui::Button::new("Connect all"))
                        .clicked()
                    {
                        self.connect_all(&ctx);
                    }
                });
            });
        });
        if !self.channels.is_empty() {
            ui.add_space(2.0);
            self.channel_chips(ui);
        }
        if !self.streams.is_empty() {
            ui.add_space(2.0);
            self.stream_chips(ui);
        }
    }

    /// One chip per watched channel: name, live/upcoming counts, rescan, stop watching.
    fn channel_chips(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut rescan = false;
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Channels:").small().color(DIM));
            for (i, w) in self.channels.iter().enumerate() {
                chip(ui, ("channel-chip", &w.target), |ui| {
                    ui.label(RichText::new("📺").small());
                    let name = if w.name.is_empty() {
                        &w.target
                    } else {
                        &w.name
                    };
                    ui.label(RichText::new(name).strong())
                        .on_hover_text(&w.target);
                    let (text, color) = if w.scanning && w.name.is_empty() {
                        ("checking…".to_string(), DIM)
                    } else if let Some(e) = &w.error {
                        (format!("error: {e}"), FLAG_ACCENT)
                    } else if w.live + w.upcoming == 0 {
                        ("nothing live or scheduled".to_string(), DIM)
                    } else {
                        (
                            format!("{} live · {} upcoming", w.live, w.upcoming),
                            MEMBER_COLOR,
                        )
                    };
                    ui.label(RichText::new(text).small().color(color));
                    if ui
                        .small_button("⟳")
                        .on_hover_text("Check for new streams now")
                        .clicked()
                    {
                        rescan = true;
                    }
                    if ui
                        .small_button("✖")
                        .on_hover_text("Stop watching this channel (removes its streams)")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                });
            }
        });
        if rescan {
            self.scan_channels(&ctx, true);
        }
        if let Some(i) = remove {
            self.unwatch_channel(i);
        }
    }

    /// One chip per stream: color, name, status, connect toggle, remove.
    fn stream_chips(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut toggle = None;
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            for (i, st) in self.streams.iter().enumerate() {
                chip(ui, ("stream-chip", st.id), |ui| {
                    ui.label(RichText::new("●").color(st.color));
                    ui.label(RichText::new(&st.label).strong())
                        .on_hover_text(&st.target);
                    let status_color = if st.status_is_error {
                        FLAG_ACCENT
                    } else if st.conn == ConnState::Live {
                        MEMBER_COLOR
                    } else {
                        DIM
                    };
                    let short = match st.conn {
                        ConnState::Idle if st.chat_disabled => "chat off",
                        ConnState::Live if st.upcoming => "waiting room",
                        ConnState::Live => "live",
                        ConnState::Connecting => "connecting…",
                        ConnState::Idle if st.status_is_error => "error",
                        ConnState::Idle => "off",
                    };
                    ui.label(RichText::new(short).small().color(status_color))
                        .on_hover_text(&st.status);
                    let (btn, tip) = if st.conn == ConnState::Idle {
                        ("▶", "Connect")
                    } else {
                        ("⏹", "Disconnect")
                    };
                    if ui.small_button(btn).on_hover_text(tip).clicked() {
                        toggle = Some(i);
                    }
                    if ui.small_button("✖").on_hover_text("Remove").clicked() {
                        remove = Some(i);
                    }
                });
            }
        });
        if let Some(i) = toggle {
            if self.streams[i].conn == ConnState::Idle {
                self.connect(i, &ctx);
            } else {
                let st = &mut self.streams[i];
                st.stop();
                st.status = "Disconnected".into();
            }
        }
        if let Some(i) = remove {
            self.remove_stream(i);
        }
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} messages · {} flagged ({} unreviewed) · {} active rules",
                    self.total_seen,
                    self.flagged.len(),
                    self.unreviewed_count(),
                    self.filters.active_rule_count()
                ))
                .color(DIM),
            );
            if !self.filters.errors.is_empty() {
                ui.label(
                    RichText::new(format!("⚠ {} list errors", self.filters.errors.len()))
                        .color(FLAG_ACCENT),
                )
                .on_hover_text(self.filters.errors.join("\n"));
            }
        });
    }

    fn chat_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Chat");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Clear").clicked() {
                    self.chat.clear();
                    self.row_heights.clear();
                }
                ui.checkbox(&mut self.auto_scroll, "Auto-scroll");
            });
        });
        ui.separator();
        let mut actions = Vec::new();
        let view = self.row_view();
        let chat = &self.chat;
        let heights = &mut self.row_heights;
        egui::ScrollArea::vertical()
            .id_salt("chat_scroll")
            .auto_shrink([false, false])
            .stick_to_bottom(self.auto_scroll)
            .show_viewport(ui, |ui, viewport| {
                ui.set_width(ui.available_width());
                // Skip rows above the view using their last measured heights.
                let mut i = 0;
                let mut skipped = 0.0;
                while i < chat.len() && skipped + heights[i] < viewport.min.y {
                    skipped += heights[i];
                    i += 1;
                }
                ui.add_space(skipped);
                let mut y = skipped;
                while i < chat.len() && y < viewport.max.y {
                    let before = ui.cursor().min.y;
                    message_row(ui, &chat[i], &view, &mut actions);
                    let h = ui.cursor().min.y - before;
                    heights[i] = h;
                    y += h;
                    i += 1;
                }
                ui.add_space(heights.range(i..).sum::<f32>());
            });
        self.note_drawn(view);
        self.apply_actions(ui.ctx(), actions);
    }

    fn flagged_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(
                RichText::new(format!("Flagged ({})", self.unreviewed_count())).color(FLAG_ACCENT),
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.show_reviewed, "Show reviewed");
            if ui.button("Mark all reviewed").clicked() {
                self.flagged.iter_mut().for_each(|f| f.reviewed = true);
            }
            if ui.button("Clear").clicked() {
                self.flagged.clear();
            }
        });
        ui.separator();

        let mut actions = Vec::new();
        let mut toggle = None;
        let view = self.row_view();
        egui::ScrollArea::vertical()
            .id_salt("flag_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Newest first: the next thing to act on is at the top.
                for (i, f) in self.flagged.iter().enumerate().rev() {
                    if f.reviewed && !self.show_reviewed {
                        continue;
                    }
                    let frame = egui::Frame::group(ui.style()).fill(if f.reviewed {
                        Color32::TRANSPARENT
                    } else {
                        FLAG_BG
                    });
                    frame.show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        message_row(ui, &f.msg, &view, &mut actions);
                        if f.name_only {
                            ui.label(
                                RichText::new(format!("username: {}", f.msg.author))
                                    .strong()
                                    .color(FLAG_ACCENT),
                            );
                        }
                        if let Some(pieces) = &f.spelled {
                            ui.label(
                                RichText::new(format!("spelled out: {pieces}"))
                                    .strong()
                                    .color(FLAG_ACCENT),
                            );
                        }
                        let rules: Vec<String> = f
                            .msg
                            .hits
                            .iter()
                            .map(|h| format!("{} → {}", h.list, h.rule))
                            .collect();
                        ui.label(
                            RichText::new(rules.join("  ·  "))
                                .small()
                                .color(FLAG_ACCENT),
                        );
                        ui.horizontal(|ui| {
                            let label = if f.reviewed { "Unmark" } else { "✔ Reviewed" };
                            if ui.small_button(label).clicked() {
                                toggle = Some(i);
                            }
                            if ui.small_button("Copy name").clicked() {
                                actions.push(RowAction::Copy(f.msg.author.clone()));
                            }
                            if can_delete(&f.msg)
                                && ui
                                    .small_button(RichText::new("🗑 Delete").color(FLAG_ACCENT))
                                    .on_hover_text("Delete this message in chat (asks first)")
                                    .clicked()
                            {
                                actions.push(RowAction::Delete(f.msg.source, f.msg.id.clone()));
                            }
                            if can_punish(&f.msg) {
                                if ui.small_button("⏱ Timeout").clicked() {
                                    actions.push(RowAction::Punish(
                                        f.msg.source,
                                        f.msg.id.clone(),
                                        Punish::Timeout,
                                    ));
                                }
                                if ui.small_button("🔨 Ban").clicked() {
                                    actions.push(RowAction::Punish(
                                        f.msg.source,
                                        f.msg.id.clone(),
                                        Punish::Ban,
                                    ));
                                }
                            }
                            if f.msg.yt_menu.is_some()
                                && !f.msg.deleted
                                && ui
                                    .small_button("☰ More")
                                    .on_hover_text("YouTube's moderation menu for this message")
                                    .clicked()
                            {
                                actions.push(RowAction::YtMenu(f.msg.source, f.msg.id.clone()));
                            }
                            if let Some(url) = f.msg.channel_url()
                                && ui.small_button("Channel").clicked()
                            {
                                actions.push(RowAction::Open(url));
                            }
                        });
                    });
                }
            });
        if let Some(i) = toggle {
            self.flagged[i].reviewed = !self.flagged[i].reviewed;
        }
        self.note_drawn(view);
        self.apply_actions(ui.ctx(), actions);
    }

    fn note_drawn(&mut self, view: RowView) {
        let now = Instant::now();
        for uri in view.drawn.into_inner() {
            self.emote_seen.insert(uri, now);
        }
    }

    /// Frees emotes that haven't been on screen for a while. egui keeps every
    /// image it ever loaded, and an animated emote is one texture per frame.
    fn sweep_emotes(&mut self, ctx: &egui::Context) {
        if self.last_emote_sweep.elapsed() < EMOTE_SWEEP_EVERY {
            return;
        }
        self.last_emote_sweep = Instant::now();
        let stale: Vec<String> = self
            .emote_seen
            .iter()
            .filter(|(_, t)| t.elapsed() > EMOTE_KEEP)
            .map(|(uri, _)| uri.clone())
            .collect();
        for uri in stale {
            self.emote_seen.remove(&uri);
            forget_emote(ctx, &uri);
        }
    }

    fn row_view(&self) -> RowView {
        RowView {
            drawn: RefCell::default(),
            show_emotes: self.cfg.show_emotes,
            mod_buttons: self.cfg.show_mod_buttons,
            // Only tag messages with their stream once chats are actually merged.
            tags: if self.streams.len() > 1 {
                self.streams
                    .iter()
                    .map(|s| (s.id, s.label.clone(), s.color))
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    fn apply_actions(&mut self, ctx: &egui::Context, actions: Vec<RowAction>) {
        for a in actions {
            match a {
                RowAction::Copy(s) => ctx.copy_text(s),
                RowAction::Open(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
                RowAction::Delete(source, id) => self.confirm_delete = Some((source, id)),
                RowAction::YtMenu(source, id) => self.open_yt_menu((source, id), ctx),
                RowAction::Punish(source, id, kind) => self.open_punish((source, id), kind, ctx),
            }
        }
    }

    fn twitch_login_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Twitch moderation");
        ui.label(
            RichText::new("Lets 🗑 Delete, ⏱ Timeout and 🔨 Ban act on Twitch chat. You must be a mod in that channel.")
                .small()
                .color(DIM),
        );
        ui.horizontal(|ui| {
            ui.label("Client ID");
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.twitch_client_id)
                    .hint_text("from dev.twitch.tv → Your Console → Applications")
                    .desired_width(300.0),
            );
        });
        let ctx = ui.ctx().clone();
        match self.twitch_auth.as_ref().map(|a| a.login.clone()) {
            Some(login) => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("✔ Logged in as {login}")).color(MEMBER_COLOR));
                    if ui.button("Log out").clicked() {
                        self.twitch_auth = None;
                        self.paths.save_twitch_auth(None);
                        self.login_status = None;
                    }
                });
                if self.twitch_auth.as_ref().is_some_and(|a| !a.can_ban()) {
                    ui.label(
                        RichText::new(
                            "This login was made before timeouts and bans were added. \
                             Log out, then log in again to allow them.",
                        )
                        .small()
                        .color(FLAG_ACCENT),
                    );
                }
            }
            None => {
                ui.horizontal(|ui| {
                    let enabled = !self.logging_in && !self.cfg.twitch_client_id.trim().is_empty();
                    if ui
                        .add_enabled(enabled, egui::Button::new("Log in with Twitch"))
                        .clicked()
                    {
                        self.logging_in = true;
                        self.login_code = None;
                        self.login_status = Some("Contacting Twitch…".into());
                        let wake = ctx.clone();
                        moderation::start_login(
                            self.cfg.twitch_client_id.trim().to_string(),
                            self.mod_tx.clone(),
                            move || wake.request_repaint(),
                        );
                    }
                    if self.logging_in && ui.button("Cancel").clicked() {
                        // The background poll times out on its own; just stop waiting on it.
                        self.logging_in = false;
                        self.login_code = None;
                        self.login_status = None;
                    }
                });
            }
        }
        if let Some((code, url)) = self.login_code.clone() {
            ui.horizontal(|ui| {
                ui.label("Go to");
                ui.hyperlink_to(&url, &url);
                ui.label("and enter:");
                ui.label(RichText::new(&code).monospace().strong().size(18.0));
                if ui.small_button("Copy").clicked() {
                    ctx.copy_text(code.clone());
                }
            });
        }
        if let Some(s) = &self.login_status {
            ui.label(RichText::new(s).small().color(DIM));
        }
    }

    fn drain_mod_events(&mut self) {
        while let Ok(ev) = self.mod_rx.try_recv() {
            match ev {
                ModEvent::LoginCode {
                    user_code,
                    verify_url,
                } => {
                    self.login_status = Some("Waiting for you to approve on Twitch…".into());
                    self.login_code = Some((user_code, verify_url));
                }
                ModEvent::LoggedIn(auth) => {
                    if !self.logging_in {
                        continue; // user cancelled
                    }
                    self.paths.save_twitch_auth(Some(&auth));
                    self.login_status = None;
                    self.twitch_auth = Some(auth);
                    self.logging_in = false;
                    self.login_code = None;
                }
                ModEvent::LoginFailed(e) => {
                    if self.logging_in {
                        self.login_status = Some(e);
                    }
                    self.logging_in = false;
                    self.login_code = None;
                }
                ModEvent::TokensRefreshed(auth) => {
                    self.paths.save_twitch_auth(Some(&auth));
                    self.twitch_auth = Some(auth);
                }
                ModEvent::YtLogin(result) => {
                    self.yt_checking = false;
                    match result {
                        Ok(auth) => {
                            self.paths.save_youtube_auth(Some(&auth));
                            self.yt_auth = Some(auth);
                            self.yt_cookie_input.clear();
                            self.yt_status = None;
                        }
                        Err(e) => self.yt_status = Some(e),
                    }
                }
                ModEvent::YtCookiesUpdated(cookies) => {
                    if let Some(a) = &mut self.yt_auth {
                        a.cookies = cookies;
                        self.paths.save_youtube_auth(Some(a));
                    }
                }
                ModEvent::YtMenu { key, result } => {
                    if let Some(p) = &mut self.punish
                        && p.key == key
                        && p.twitch.is_none()
                    {
                        p.yt = Some(match &result {
                            Ok(items) => yt_mod::find_punish(items, p.kind == Punish::Ban)
                                .cloned()
                                .ok_or_else(|| {
                                    let labels: Vec<&str> =
                                        items.iter().map(|i| i.label.as_str()).collect();
                                    format!(
                                        "YouTube didn't offer a {} option for this message. \
                                         Are you a moderator in this channel? Its menu had: {}",
                                        p.kind.verb().to_lowercase(),
                                        labels.join(", ")
                                    )
                                }),
                            Err(e) => Err(e.clone()),
                        });
                    }
                    if let Some(v) = &mut self.yt_menu
                        && v.key == key
                    {
                        v.items = Some(result);
                    }
                }
                ModEvent::ActionDone { key, result } => {
                    let status = match result {
                        Ok(t) => ModStatus::Info(t),
                        Err(e) => ModStatus::Failed(e),
                    };
                    self.for_message(&key, |m| m.mod_status = status.clone());
                }
                ModEvent::YtActionDone {
                    key,
                    is_delete,
                    result,
                } => {
                    if is_delete {
                        let failed = result.as_ref().err().cloned();
                        self.for_message(&key, |m| match &failed {
                            None => {
                                m.deleted = true;
                                m.mod_status = ModStatus::None;
                            }
                            Some(e) => m.mod_status = ModStatus::Failed(e.clone()),
                        });
                    }
                    if !is_delete {
                        let status = match &result {
                            Ok(t) => ModStatus::Info(t.clone()),
                            Err(e) => ModStatus::Failed(e.clone()),
                        };
                        self.for_message(&key, |m| m.mod_status = status.clone());
                    }
                    if let Some(v) = &mut self.yt_menu
                        && v.key == key
                    {
                        v.note = Some(result);
                    }
                }
                ModEvent::DeleteDone { key, result } => {
                    let status = match result {
                        // Twitch confirms with a CLEARMSG in chat, which strikes the
                        // message through; mark it now so the UI doesn't lag behind.
                        Ok(()) => None,
                        Err(e) => Some(ModStatus::Failed(e)),
                    };
                    self.for_message(&key, |m| match &status {
                        None => {
                            m.deleted = true;
                            m.mod_status = ModStatus::None;
                        }
                        Some(s) => m.mod_status = s.clone(),
                    });
                }
            }
        }
    }

    /// Applies `f` to a message in both the chat and the Flagged panel.
    fn for_message(&mut self, key: &(u64, String), mut f: impl FnMut(&mut ChatMsg)) {
        for m in self
            .chat
            .iter_mut()
            .filter(|m| m.source == key.0 && m.id == key.1)
        {
            f(m);
        }
        for e in self
            .flagged
            .iter_mut()
            .filter(|e| e.msg.source == key.0 && e.msg.id == key.1)
        {
            f(&mut e.msg);
            if e.msg.deleted {
                e.reviewed = true;
            }
        }
    }

    fn find_message(&self, key: &(u64, String)) -> Option<&ChatMsg> {
        self.chat
            .iter()
            .chain(self.flagged.iter().map(|e| &e.msg))
            .find(|m| m.source == key.0 && m.id == key.1)
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(key) = self.confirm_delete.clone() else {
            return;
        };
        let Some(m) = self.find_message(&key).cloned() else {
            self.confirm_delete = None;
            return;
        };
        let is_youtube = m.room_id.is_none();
        let logged_in = if is_youtube {
            self.yt_auth.is_some()
        } else {
            self.twitch_auth.is_some()
        };
        let platform = if is_youtube { "YouTube" } else { "Twitch" };
        let resp = egui::Modal::new(egui::Id::new("confirm_delete")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Delete this message?");
            ui.add_space(4.0);
            ui.label(RichText::new(format!("{}:", m.author)).strong());
            let preview: String = m.text.chars().take(200).collect();
            ui.label(preview);
            ui.add_space(8.0);
            let mut choice = None;
            if logged_in {
                ui.label(
                    RichText::new(format!(
                        "This removes it from {platform} chat for everyone."
                    ))
                    .small()
                    .color(DIM),
                );
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("🗑 Delete").color(Color32::WHITE))
                                .fill(STOP_FILL),
                        )
                        .clicked()
                    {
                        choice = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(false);
                    }
                });
            } else {
                ui.label(
                    RichText::new(format!(
                        "Log in to {platform} first: Settings → {platform} moderation."
                    ))
                    .color(FLAG_ACCENT),
                );
                ui.horizontal(|ui| {
                    if ui.button("Open Settings").clicked() {
                        self.show_settings = true;
                        choice = Some(false);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(false);
                    }
                });
            }
            choice
        });
        match resp.inner {
            Some(true) => {
                self.confirm_delete = None;
                self.send_delete(&m, ctx);
            }
            Some(false) => self.confirm_delete = None,
            None if resp.should_close() => self.confirm_delete = None,
            None => {}
        }
    }

    fn send_delete(&mut self, m: &ChatMsg, ctx: &egui::Context) {
        if let (Some(auth), Some(params)) = (self.yt_auth.clone(), m.yt_menu.clone()) {
            let key = (m.source, m.id.clone());
            self.for_message(&key, |x| x.mod_status = ModStatus::Deleting);
            let wake = ctx.clone();
            yt_mod::delete_message(auth.cookies, params, key, self.mod_tx.clone(), move || {
                wake.request_repaint()
            });
            return;
        }
        let (Some(auth), Some(room)) = (self.twitch_auth.clone(), m.room_id.clone()) else {
            return;
        };
        let key = (m.source, m.id.clone());
        self.for_message(&key, |x| x.mod_status = ModStatus::Deleting);
        let wake = ctx.clone();
        moderation::delete_twitch_message(
            self.cfg.twitch_client_id.trim().to_string(),
            auth,
            room,
            m.id.clone(),
            key,
            self.mod_tx.clone(),
            move || wake.request_repaint(),
        );
    }

    fn youtube_login_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("YouTube moderation");
        ui.label(
            RichText::new(
                "Lets 🗑 Delete, ⏱ Timeout, 🔨 Ban and ☰ More act on YouTube messages as you. Chat is still read without logging in.",
            )
            .small()
            .color(DIM),
        );
        match self.yt_auth.as_ref().map(|a| a.account.clone()) {
            Some(account) => {
                ui.horizontal(|ui| {
                    let who = if account.is_empty() {
                        "✔ Logged in".to_string()
                    } else {
                        format!("✔ Logged in as {account}")
                    };
                    ui.label(RichText::new(who).color(MEMBER_COLOR));
                    if ui.button("Log out").clicked() {
                        self.yt_auth = None;
                        self.paths.save_youtube_auth(None);
                        self.yt_status = None;
                    }
                });
                ui.label(
                    RichText::new("If actions start saying the login was rejected, log out and paste a fresh cookie.")
                        .small()
                        .color(DIM),
                );
            }
            None => {
                ui.collapsing("How to get your cookie", |ui| {
                    for step in [
                        "1. In Chrome or Edge, open youtube.com and make sure you're logged in as the account that mods.",
                        "2. Press F12, open the Network tab, then press F5 to reload.",
                        "3. Click the first row (www.youtube.com) and find Request Headers → cookie.",
                        "4. Right-click the cookie value → Copy value, and paste it below.",
                        "This is your Google login: it's saved only on this PC (youtube_login.json). Never share it.",
                    ] {
                        ui.label(RichText::new(step).small());
                    }
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.yt_cookie_input)
                        .password(true)
                        .hint_text("paste cookie value here")
                        .desired_width(f32::INFINITY),
                );
                let ctx = ui.ctx().clone();
                let ready = !self.yt_checking && !self.yt_cookie_input.trim().is_empty();
                if ui
                    .add_enabled(ready, egui::Button::new("Log in with this cookie"))
                    .clicked()
                {
                    self.yt_checking = true;
                    self.yt_status = Some("Checking with YouTube…".into());
                    let wake = ctx.clone();
                    yt_mod::check_login(
                        self.yt_cookie_input.trim().to_string(),
                        self.mod_tx.clone(),
                        move || wake.request_repaint(),
                    );
                }
            }
        }
        if let Some(s) = &self.yt_status {
            ui.label(RichText::new(s).small().color(FLAG_ACCENT));
        }
    }

    fn open_yt_menu(&mut self, key: (u64, String), ctx: &egui::Context) {
        let Some(m) = self.find_message(&key).cloned() else {
            return;
        };
        let mut view = YtMenuView {
            key: key.clone(),
            author: m.author.clone(),
            text: m.text.chars().take(200).collect(),
            items: None,
            note: None,
        };
        match (&self.yt_auth, &m.yt_menu) {
            (Some(auth), Some(params)) => {
                let wake = ctx.clone();
                yt_mod::fetch_menu(
                    auth.cookies.clone(),
                    params.clone(),
                    key,
                    self.mod_tx.clone(),
                    move || wake.request_repaint(),
                );
            }
            _ => {
                view.items = Some(Err(
                    "Log in to YouTube first: Settings → YouTube moderation.".into(),
                ));
            }
        }
        self.yt_menu = Some(view);
    }

    fn yt_menu_window(&mut self, ctx: &egui::Context) {
        let Some(view) = &self.yt_menu else {
            return;
        };
        let mut open = true;
        let mut picked: Option<PendingAction> = None;
        let mut open_settings = false;
        egui::Window::new("☰ Message actions")
            .id(egui::Id::new("yt_menu_window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.label(RichText::new(format!("{}:", view.author)).strong());
                ui.label(&view.text);
                ui.separator();
                match &view.items {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Asking YouTube for this message's menu…");
                        });
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(FLAG_ACCENT));
                        if e.contains("Settings") && ui.button("Open Settings").clicked() {
                            open_settings = true;
                        }
                    }
                    Some(Ok(items)) => {
                        for item in items {
                            if item.actions.is_empty() {
                                ui.add_enabled(false, egui::Button::new(&item.label))
                                    .on_disabled_hover_text(
                                        "This one has to be done on YouTube itself.",
                                    );
                                continue;
                            }
                            if item.actions.len() == 1 {
                                let label = if item.is_delete() {
                                    RichText::new(&item.label).color(FLAG_ACCENT)
                                } else {
                                    RichText::new(&item.label)
                                };
                                if ui.button(label).clicked() {
                                    picked = Some(PendingAction {
                                        key: view.key.clone(),
                                        author: view.author.clone(),
                                        what: item.label.clone(),
                                        action: item.actions[0].clone(),
                                        is_delete: item.is_delete(),
                                    });
                                }
                                continue;
                            }
                            ui.menu_button(format!("{}…", item.label), |ui| {
                                for a in &item.actions {
                                    if ui.button(&a.label).clicked() {
                                        picked = Some(PendingAction {
                                            key: view.key.clone(),
                                            author: view.author.clone(),
                                            what: format!("{}: {}", item.label, a.label),
                                            action: a.clone(),
                                            is_delete: false,
                                        });
                                        ui.close();
                                    }
                                }
                            });
                        }
                    }
                }
                match &view.note {
                    Some(Ok(t)) => {
                        ui.separator();
                        ui.label(RichText::new(format!("✔ {t}")).color(MEMBER_COLOR));
                    }
                    Some(Err(e)) => {
                        ui.separator();
                        ui.label(RichText::new(e).color(FLAG_ACCENT));
                    }
                    None => {}
                }
            });
        if open_settings {
            self.show_settings = true;
        }
        if picked.is_some() {
            self.confirm_action = picked;
        }
        if !open {
            self.yt_menu = None;
        }
    }

    fn action_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = self.confirm_action.clone() else {
            return;
        };
        let resp = egui::Modal::new(egui::Id::new("confirm_yt_action")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading(format!("{}?", p.what));
            ui.label(format!("On {}'s message, on YouTube, as you.", p.author));
            ui.add_space(8.0);
            let mut choice = None;
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new(&p.what).color(Color32::WHITE))
                            .fill(STOP_FILL),
                    )
                    .clicked()
                {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
            choice
        });
        match resp.inner {
            Some(true) => {
                self.confirm_action = None;
                if let Some(auth) = self.yt_auth.clone() {
                    if p.is_delete {
                        self.for_message(&p.key, |x| x.mod_status = ModStatus::Deleting);
                    }
                    if let Some(v) = &mut self.yt_menu {
                        v.note = None;
                    }
                    let wake = ctx.clone();
                    yt_mod::run_action(
                        auth.cookies,
                        p.action,
                        p.key,
                        p.is_delete,
                        self.mod_tx.clone(),
                        move || wake.request_repaint(),
                    );
                }
            }
            Some(false) => self.confirm_action = None,
            None if resp.should_close() => self.confirm_action = None,
            None => {}
        }
    }

    fn open_punish(&mut self, key: (u64, String), kind: Punish, ctx: &egui::Context) {
        let Some(m) = self.find_message(&key).cloned() else {
            return;
        };
        let twitch = m.room_id.clone().map(|r| (r, m.channel_id.clone()));
        let mut view = PunishView {
            key: key.clone(),
            kind,
            author: m.author.clone(),
            text: m.text.chars().take(200).collect(),
            twitch: twitch.clone(),
            reason: String::new(),
            yt: None,
            yt_pick: 0,
        };
        if twitch.is_none() {
            match (&self.yt_auth, &m.yt_menu) {
                (Some(auth), Some(params)) => {
                    let wake = ctx.clone();
                    yt_mod::fetch_menu(
                        auth.cookies.clone(),
                        params.clone(),
                        key,
                        self.mod_tx.clone(),
                        move || wake.request_repaint(),
                    );
                }
                _ => {
                    view.yt = Some(Err(
                        "Log in to YouTube first: Settings → YouTube moderation.".into(),
                    ));
                }
            }
        }
        self.punish = Some(view);
    }

    fn punish_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut p) = self.punish.take() else {
            return;
        };
        let verb = p.kind.verb();
        let twitch_login = match &self.twitch_auth {
            None => Some("Log in to Twitch first: Settings → Twitch moderation."),
            Some(a) if !a.can_ban() => Some(
                "Your Twitch login was made before timeouts and bans were added. \
                 Log out and log in again in Settings → Twitch moderation.",
            ),
            Some(_) => None,
        };
        let timeout_secs = &mut self.cfg.twitch_timeout_secs;
        let mut choice: Option<bool> = None;
        let mut open_settings = false;
        let resp = egui::Modal::new(egui::Id::new("confirm_punish")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.heading(format!("{verb} {}?", p.author));
            if !p.text.is_empty() {
                ui.label(RichText::new(&p.text).small().color(DIM));
            }
            ui.add_space(6.0);
            let mut ready = false;
            let mut needs_settings = false;
            let mut button = verb.to_string();
            if p.twitch.is_some() {
                if let Some(problem) = twitch_login {
                    ui.label(RichText::new(problem).color(FLAG_ACCENT));
                    needs_settings = true;
                } else {
                    if p.kind == Punish::Timeout {
                        ui.horizontal_wrapped(|ui| {
                            for (secs, label) in TIMEOUT_CHOICES {
                                ui.selectable_value(&mut *timeout_secs, secs, label);
                            }
                        });
                        button = format!("⏱ Timeout {}", moderation::human_duration(*timeout_secs));
                    } else {
                        ui.label(
                            RichText::new(
                                "Bans them from this Twitch chat until someone unbans them.",
                            )
                            .small()
                            .color(DIM),
                        );
                        button = "🔨 Ban".into();
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut p.reason)
                            .hint_text("Reason (optional, shown in Twitch's mod log)")
                            .desired_width(f32::INFINITY),
                    );
                    ready = true;
                }
            } else {
                match &p.yt {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Asking YouTube what you can do here…");
                        });
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(FLAG_ACCENT));
                        needs_settings = e.contains("Settings");
                    }
                    Some(Ok(item)) if item.actions.is_empty() => {
                        ui.label(
                            RichText::new(format!(
                                "YouTube only allows \"{}\" on its own site for this message.",
                                item.label
                            ))
                            .color(FLAG_ACCENT),
                        );
                    }
                    Some(Ok(item)) => {
                        ui.label(format!("YouTube: {}", item.label));
                        if item.actions.len() > 1 {
                            for (i, a) in item.actions.iter().enumerate() {
                                ui.radio_value(&mut p.yt_pick, i, &a.label);
                            }
                        }
                        let icon = if p.kind == Punish::Ban { "🔨" } else { "⏱" };
                        button = format!("{icon} {}", item.label);
                        ready = true;
                    }
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ready
                    && ui
                        .add(
                            egui::Button::new(RichText::new(&button).color(Color32::WHITE))
                                .fill(STOP_FILL),
                        )
                        .clicked()
                {
                    choice = Some(true);
                }
                if needs_settings && ui.button("Open Settings").clicked() {
                    open_settings = true;
                    choice = Some(false);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        let dismissed = resp.should_close();
        if open_settings {
            self.show_settings = true;
        }
        match choice {
            Some(true) => self.run_punish(p, ctx),
            Some(false) => {}
            None if dismissed => {}
            None => self.punish = Some(p),
        }
    }

    fn run_punish(&mut self, p: PunishView, ctx: &egui::Context) {
        let wake = ctx.clone();
        let busy = match p.kind {
            Punish::Timeout => "timing out…",
            Punish::Ban => "banning…",
        };
        if let Some((room, user)) = p.twitch.clone() {
            let Some(auth) = self.twitch_auth.clone() else {
                return;
            };
            self.for_message(&p.key, |m| m.mod_status = ModStatus::Busy(busy.into()));
            let secs = (p.kind == Punish::Timeout).then_some(self.cfg.twitch_timeout_secs);
            moderation::twitch_ban(
                self.cfg.twitch_client_id.trim().to_string(),
                auth,
                room,
                user,
                secs,
                p.reason.clone(),
                p.key.clone(),
                self.mod_tx.clone(),
                move || wake.request_repaint(),
            );
        } else if let (Some(auth), Some(Ok(item))) = (self.yt_auth.clone(), &p.yt) {
            let Some(action) = item
                .actions
                .get(p.yt_pick)
                .or(item.actions.first())
                .cloned()
            else {
                return;
            };
            self.for_message(&p.key, |m| m.mod_status = ModStatus::Busy(busy.into()));
            yt_mod::run_action(
                auth.cookies,
                action,
                p.key.clone(),
                false,
                self.mod_tx.clone(),
                move || wake.request_repaint(),
            );
        }
    }

    fn quit_dialog(&mut self, ctx: &egui::Context) {
        let resp = egui::Modal::new(egui::Id::new("confirm_quit")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.heading("Stop Chat Mod?");
            ui.label("Disconnects every chat and closes the program. Your streams, channels and word lists are saved.");
            ui.add_space(8.0);
            let mut choice = None;
            ui.horizontal(|ui| {
                if ui.button(RichText::new("⏻ Stop").color(FLAG_ACCENT)).clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
            choice
        });
        match resp.inner {
            Some(true) => self.quit(ctx),
            Some(false) => self.confirm_quit = false,
            None if resp.should_close() => self.confirm_quit = false,
            None => {}
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("YouTube API key");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.api_key)
                                .password(true)
                                .desired_width(280.0),
                        );
                        ui.end_row();

                        ui.label("Chat refresh (ms)");
                    ui.add(
                        egui::DragValue::new(&mut self.cfg.innertube_poll_ms)
                            .range(250..=5000)
                            .speed(50),
                    )
                    .on_hover_text("InnerTube only. How often to check for new messages. Takes effect on reconnect.");
                    ui.end_row();

                    ui.label("API min poll (sec)");
                        ui.add(egui::DragValue::new(&mut self.cfg.api_min_poll_secs).range(1..=60))
                            .on_hover_text(
                                "Each API poll costs ~5 quota units. Default quota is 10,000/day.",
                            );
                        ui.end_row();

                        ui.label("Flag sound");
                        ui.checkbox(&mut self.cfg.sound_enabled, "Play sound on flag");
                        ui.end_row();

                        ui.label("Custom sound file");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.cfg.sound_file)
                                    .hint_text("blank = system sound (.wav/.oga)")
                                    .desired_width(220.0),
                            );
                            if ui.button("Test").clicked() {
                                sound::play(&self.cfg.sound_file);
                            }
                        });
                        ui.end_row();

                        ui.label("Channel rescan");
                        ui.add(
                            egui::DragValue::new(&mut self.cfg.channel_rescan_secs)
                                .range(30..=900)
                                .suffix(" s"),
                        )
                        .on_hover_text("How often watched channels are checked for new or ended streams.");
                        ui.end_row();

                        ui.label("Emotes");
                        ui.checkbox(&mut self.cfg.show_emotes, "Show emoji and channel emotes as images");
                        ui.end_row();

                        ui.label("Spelled-out words");
                        ui.checkbox(
                            &mut self.cfg.catch_spelled,
                            "Catch words typed a letter or two per message",
                        )
                        .on_hover_text(format!(
                            "Joins each person's tiny messages (up to {} letters) from the last {}s \
                             and checks them against your word lists.",
                            crate::spelled::MAX_FRAGMENT_LETTERS,
                            crate::spelled::SPELL_WINDOW.as_secs()
                        ));
                        ui.end_row();

                        ui.label("Repeated messages");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.cfg.catch_repeats, "Same person")
                                .on_hover_text(format!(
                                    "Flags someone sending the same message over and over, \
                                     even with different caps, punctuation or stretched letters. \
                                     Messages under {} letters (lol, gg) are ignored.",
                                    crate::repeats::MIN_LETTERS
                                ));
                            ui.add_enabled(
                                self.cfg.catch_repeats,
                                egui::DragValue::new(&mut self.cfg.repeat_count)
                                    .range(2..=20)
                                    .suffix("×"),
                            );
                            ui.label("in");
                            ui.add_enabled(
                                self.cfg.catch_repeats,
                                egui::DragValue::new(&mut self.cfg.repeat_window_secs)
                                    .range(5..=600)
                                    .suffix(" s"),
                            );
                        });
                        ui.end_row();

                        ui.label("Copy-paste raids");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.cfg.catch_raids, "Many people")
                                .on_hover_text(
                                    "Flags lots of different people posting the same message \
                                     at once, the usual pattern of spam bots.",
                                );
                            ui.add_enabled(
                                self.cfg.catch_raids,
                                egui::DragValue::new(&mut self.cfg.raid_count)
                                    .range(2..=50)
                                    .suffix(" people"),
                            );
                            ui.label("in");
                            ui.add_enabled(
                                self.cfg.catch_raids,
                                egui::DragValue::new(&mut self.cfg.raid_window_secs)
                                    .range(5..=600)
                                    .suffix(" s"),
                            );
                        });
                        ui.end_row();

                        ui.label("Exemptions");
                        ui.checkbox(
                            &mut self.cfg.ignore_mods,
                            "Don't flag moderators (owner is never flagged)",
                        );
                        ui.end_row();

                        ui.label("Chat history");
                        ui.add(
                            egui::DragValue::new(&mut self.cfg.max_messages)
                                .range(100..=20_000)
                                .suffix(" msgs"),
                        );
                        ui.end_row();

                        ui.label("Log");
                        ui.checkbox(
                            &mut self.cfg.log_flagged,
                            "Append flagged messages to flagged.log",
                        );
                        ui.end_row();

                        ui.label("Mod buttons");
                        ui.checkbox(
                            &mut self.cfg.show_mod_buttons,
                            "Show 🗑 ⏱ 🔨 on every chat message",
                        );
                        ui.end_row();
                    });
                ui.separator();
                self.twitch_login_section(ui);
                ui.separator();
                self.youtube_login_section(ui);
                ui.separator();
                ui.label(
                    RichText::new(format!("Data folder: {}", self.paths.dir.display()))
                        .small()
                        .color(DIM),
                );
                if ui.button("Open data folder").clicked() {
                    open_path(&self.paths.dir);
                }
            });
        self.show_settings = open;
    }

    fn lists_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_lists;
        let mut toggles = Vec::new();
        let mut name_toggles: Vec<(String, bool)> = Vec::new();
        let mut edit = None;
        egui::Window::new("Word lists")
            .open(&mut open)
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Reload from disk").clicked() {
                        self.reload_filters();
                    }
                    if ui.button("Open lists folder").clicked() {
                        open_path(&self.paths.lists_dir);
                    }
                });
                ui.separator();
                egui::Grid::new("lists_grid")
                    .num_columns(4)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        for l in &self.filters.lists {
                            let mut on = l.enabled;
                            if ui.checkbox(&mut on, &l.name).changed() {
                                toggles.push((l.name.clone(), on));
                            }
                            ui.label(RichText::new(format!("{} rules", l.rule_count())).color(DIM));
                            let mut names = l.check_names;
                            if ui
                                .checkbox(&mut names, "usernames")
                                .on_hover_text(
                                    "Also flag anyone whose NAME contains one of this list's words \
                                     (e.g. CoolXyzGuy). Exceptions go in names_ok.txt.",
                                )
                                .changed()
                            {
                                name_toggles.push((l.name.clone(), names));
                            }
                            if ui.small_button("Edit").clicked() {
                                edit = Some(l.name.clone());
                            }
                            ui.end_row();
                        }
                    });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_list_name)
                            .hint_text("new list name")
                            .desired_width(160.0),
                    );
                    if ui.button("Create").clicked() {
                        let name: String = self
                            .new_list_name
                            .trim()
                            .chars()
                            .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' '))
                            .collect();
                        if !name.is_empty() {
                            let path = self.paths.list_path(&name);
                            if !path.exists() {
                                let _ = fs::write(&path, LIST_HEADER);
                            }
                            self.new_list_name.clear();
                            self.reload_filters();
                            edit = Some(name);
                        }
                    }
                });
                for e in &self.filters.errors {
                    ui.label(RichText::new(format!("⚠ {e}")).color(FLAG_ACCENT));
                }

                ui.separator();
                ui.label("Test a message against the enabled lists:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.test_text).desired_width(f32::INFINITY),
                );
                if !self.test_text.is_empty() {
                    let hits = self.filters.check("", "", &self.test_text);
                    if hits.is_empty() {
                        ui.label(RichText::new("No match").color(MEMBER_COLOR));
                    } else {
                        for h in hits {
                            ui.label(
                                RichText::new(format!("FLAG  {} → {}", h.list, h.rule))
                                    .color(FLAG_ACCENT),
                            );
                        }
                    }
                    ui.label(
                        RichText::new(format!(
                            "normalized: \"{}\"",
                            crate::filter::normalize(&self.test_text).trim()
                        ))
                        .small()
                        .color(DIM),
                    );
                }

                ui.add_space(6.0);
                ui.label("Test a username:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.name_test)
                        .hint_text("e.g. xX_SomeName_Xx")
                        .desired_width(f32::INFINITY),
                );
                if !self.name_test.trim().is_empty() {
                    let mut hits = self.filters.check_name(&self.name_test);
                    // name: rules live in the per-message check.
                    hits.extend(
                        self.filters
                            .check(&self.name_test, "", "")
                            .into_iter()
                            .filter(|h| h.rule.starts_with("name:") || h.rule.starts_with("user:")),
                    );
                    if hits.is_empty() {
                        ui.label(RichText::new("No match").color(MEMBER_COLOR));
                    } else {
                        for h in hits {
                            ui.label(
                                RichText::new(format!("FLAG  {} → {}", h.list, h.rule))
                                    .color(FLAG_ACCENT),
                            );
                        }
                    }
                    ui.label(
                        RichText::new(format!(
                            "folded: \"{}\"",
                            crate::filter::fold_name(&self.name_test)
                        ))
                        .small()
                        .color(DIM),
                    );
                }

                ui.add_space(6.0);
                ui.label("Test a word spelled across messages (one message per line):");
                ui.add(
                    egui::TextEdit::multiline(&mut self.spell_test)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                if !self.spell_test.trim().is_empty() {
                    let mut tracker = SpellTracker::default();
                    let now = Instant::now();
                    let hit = self.spell_test.lines().enumerate().find_map(|(i, line)| {
                        tracker.push(&self.filters, (0, "test".into()), &i.to_string(), line, now)
                    });
                    match hit {
                        Some(h) => {
                            for f in h.hits {
                                ui.label(
                                    RichText::new(format!("FLAG  {} → {}", f.list, f.rule))
                                        .color(FLAG_ACCENT),
                                );
                            }
                            ui.label(
                                RichText::new(format!("pieces: {}", h.combined))
                                    .small()
                                    .color(DIM),
                            );
                        }
                        None => {
                            ui.label(RichText::new("No match").color(MEMBER_COLOR));
                        }
                    }
                }
            });
        self.show_lists = open;

        let names_changed = !name_toggles.is_empty();
        for (name, on) in name_toggles {
            self.cfg.name_check_lists.retain(|n| *n != name);
            if on {
                self.cfg.name_check_lists.push(name);
            }
        }
        if names_changed {
            self.reload_filters();
        }
        for (name, on) in toggles {
            self.filters.set_enabled(&name, on);
            self.cfg.disabled_lists.retain(|n| *n != name);
            if !on {
                self.cfg.disabled_lists.push(name);
            }
        }
        if let Some(name) = edit {
            let text = fs::read_to_string(self.paths.list_path(&name)).unwrap_or_default();
            self.editing_list = Some((name, text));
        }
        self.list_editor(ctx);
    }

    fn list_editor(&mut self, ctx: &egui::Context) {
        let Some((name, text)) = self.editing_list.as_mut() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        egui::Window::new(format!("Edit list: {name}"))
            .open(&mut open)
            .default_size([460.0, 420.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    save = ui.button("Save").clicked();
                    ui.label(RichText::new("Ctrl+S also saves").small().color(DIM));
                });
                save |= ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .code_editor()
                            .desired_width(f32::INFINITY)
                            .desired_rows(20),
                    );
                });
            });
        if save {
            let (name, text) = self.editing_list.as_ref().unwrap();
            let _ = fs::write(self.paths.list_path(name), text);
            self.reload_filters();
        }
        if !open {
            self.editing_list = None;
        }
    }
}

/// A framed chip in a wrapping row. egui can't know a framed group's width before
/// laying it out, so it would run off the edge; reuse last frame's width to
/// start a new line when the chip won't fit.
fn chip(
    ui: &mut egui::Ui,
    key: impl std::hash::Hash + std::fmt::Debug,
    add: impl FnOnce(&mut egui::Ui),
) {
    let id = ui.id().with(key);
    let last_width: f32 = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(0.0);
    let at_line_start = ui.cursor().min.x <= ui.max_rect().min.x + 1.0;
    if !at_line_start && last_width > ui.available_size_before_wrap().x {
        ui.end_row();
    }
    let resp = egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| ui.horizontal(add));
    let width = resp.response.rect.width();
    ui.ctx().data_mut(|d| d.insert_temp(id, width));
}

enum RowAction {
    Copy(String),
    Open(String),
    /// Ask to delete (stream id, message id). Nothing is sent until confirmed.
    Delete(u64, String),
    /// Open YouTube's ⋮ menu for (stream id, message id).
    YtMenu(u64, String),
    /// Ask to timeout/ban the author of (stream id, message id).
    Punish(u64, String, Punish),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Punish {
    Timeout,
    Ban,
}

impl Punish {
    fn verb(self) -> &'static str {
        match self {
            Punish::Timeout => "Timeout",
            Punish::Ban => "Ban",
        }
    }
}

const TIMEOUT_CHOICES: [(u32, &str); 6] = [
    (60, "1 min"),
    (300, "5 min"),
    (600, "10 min"),
    (3600, "1 hour"),
    (86_400, "1 day"),
    (604_800, "1 week"),
];

struct PunishView {
    key: (u64, String),
    kind: Punish,
    author: String,
    text: String,
    /// Twitch: (room id, user id). None for YouTube.
    twitch: Option<(String, String)>,
    reason: String,
    /// YouTube: the matching ⋮ item, None while loading.
    yt: Option<Result<MenuItem, String>>,
    yt_pick: usize,
}

struct YtMenuView {
    key: (u64, String),
    author: String,
    text: String,
    /// None while loading.
    items: Option<Result<Vec<MenuItem>, String>>,
    /// Result of the last action run from this window.
    note: Option<Result<String, String>>,
}

#[derive(Clone)]
struct PendingAction {
    key: (u64, String),
    author: String,
    what: String,
    action: MenuAction,
    is_delete: bool,
}

struct RowView {
    /// Emote URIs drawn this frame, for `sweep_emotes`.
    drawn: RefCell<Vec<String>>,
    show_emotes: bool,
    mod_buttons: bool,
    tags: Vec<(u64, String, Color32)>,
}

fn message_row(ui: &mut egui::Ui, m: &ChatMsg, view: &RowView, actions: &mut Vec<RowAction>) {
    let flagged = m.is_flagged();
    let frame = egui::Frame::new()
        .fill(if flagged {
            FLAG_BG
        } else {
            Color32::TRANSPARENT
        })
        .inner_margin(egui::Margin::symmetric(4, 2));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(RichText::new(&m.time).small().color(DIM));
            if let Some((_, label, color)) = view.tags.iter().find(|(id, ..)| *id == m.source) {
                ui.label(RichText::new(format!("[{label}]")).small().color(*color));
            }
            if view.mod_buttons && can_punish(m) {
                let icon = |t: &str| egui::Button::new(RichText::new(t).small()).frame(false);
                // Disabled rather than hidden so the row doesn't shift once deleted.
                if ui
                    .add_enabled(can_delete(m), icon("🗑"))
                    .on_hover_text("Delete message…")
                    .clicked()
                {
                    actions.push(RowAction::Delete(m.source, m.id.clone()));
                }
                if ui.add(icon("⏱")).on_hover_text("Timeout user…").clicked() {
                    actions.push(RowAction::Punish(m.source, m.id.clone(), Punish::Timeout));
                }
                if ui.add(icon("🔨")).on_hover_text("Ban user…").clicked() {
                    actions.push(RowAction::Punish(m.source, m.id.clone(), Punish::Ban));
                }
            }
            if flagged {
                ui.label(RichText::new("⚑").color(FLAG_ACCENT).strong());
            }
            if let Some(sc) = &m.superchat {
                ui.label(
                    RichText::new(format!("[{sc}]"))
                        .color(SUPERCHAT_COLOR)
                        .strong(),
                );
            }
            let name_color = if m.is_owner {
                OWNER_COLOR
            } else if m.is_mod {
                MOD_COLOR
            } else if m.is_member {
                MEMBER_COLOR
            } else {
                Color32::from_gray(190)
            };
            let shown = if m.is_verified {
                format!("{} ✔:", m.author)
            } else {
                format!("{}:", m.author)
            };
            let mut name = RichText::new(shown).color(name_color).strong();
            if m.name_flagged {
                name = name.color(Color32::WHITE).background_color(NAME_FLAG_BG);
            }
            let resp = ui.add(egui::Label::new(name).sense(egui::Sense::click()));
            resp.context_menu(|ui| {
                if ui.button("Copy name").clicked() {
                    actions.push(RowAction::Copy(m.author.clone()));
                    ui.close();
                }
                if ui.button("Copy message").clicked() {
                    actions.push(RowAction::Copy(m.text.clone()));
                    ui.close();
                }
                if can_delete(m) {
                    ui.separator();
                    if ui
                        .button(RichText::new("🗑 Delete message…").color(FLAG_ACCENT))
                        .clicked()
                    {
                        actions.push(RowAction::Delete(m.source, m.id.clone()));
                        ui.close();
                    }
                }
                if can_punish(m) {
                    if ui.button("⏱ Timeout user…").clicked() {
                        actions.push(RowAction::Punish(m.source, m.id.clone(), Punish::Timeout));
                        ui.close();
                    }
                    if ui.button("🔨 Ban user…").clicked() {
                        actions.push(RowAction::Punish(m.source, m.id.clone(), Punish::Ban));
                        ui.close();
                    }
                }
                if m.yt_menu.is_some() && !m.deleted && ui.button("☰ More actions…").clicked() {
                    actions.push(RowAction::YtMenu(m.source, m.id.clone()));
                    ui.close();
                }
                if let Some(url) = m.channel_url()
                    && ui.button("Open channel").clicked()
                {
                    actions.push(RowAction::Open(url));
                    ui.close();
                }
            });
            message_body(ui, m, view);
            if m.deleted {
                ui.label(RichText::new("(removed)").small().color(DIM));
            }
            match &m.mod_status {
                ModStatus::None => {}
                ModStatus::Deleting => {
                    ui.label(RichText::new("deleting…").small().color(DIM));
                }
                ModStatus::Busy(s) => {
                    ui.label(RichText::new(s).small().color(DIM));
                }
                ModStatus::Info(s) => {
                    ui.label(RichText::new(format!("✔ {s}")).small().color(MEMBER_COLOR));
                }
                ModStatus::Failed(e) => {
                    ui.label(RichText::new("✖ failed").small().color(FLAG_ACCENT))
                        .on_hover_text(e);
                }
            }
        });
    });
}

/// Timeout/ban is offered for Twitch and YouTube chatters, except the
/// streamer and other mods (neither platform allows it).
fn can_punish(m: &ChatMsg) -> bool {
    (m.room_id.is_some() || m.yt_menu.is_some())
        && !m.is_owner
        && !m.is_mod
        && !matches!(m.mod_status, ModStatus::Busy(_))
}

/// Delete is offered for Twitch and YouTube messages that aren't already gone.
fn can_delete(m: &ChatMsg) -> bool {
    (m.room_id.is_some() || m.yt_menu.is_some())
        && !m.deleted
        && m.mod_status != ModStatus::Deleting
}

/// Drops every cached copy of an emote: its bytes, decoded frames, and the
/// per-frame textures (animated images are cached as "uri#0", "uri#1", ...).
fn forget_emote(ctx: &egui::Context, uri: &str) {
    let id = egui::Id::new(uri);
    let frames = ctx
        .data(|d| d.get_temp::<egui::FrameDurations>(id))
        .map_or(0, |f| f.all().count());
    ctx.forget_image(uri);
    for i in 0..frames.max(1) {
        ctx.forget_image(&format!("{uri}#{i}"));
    }
    ctx.data_mut(|d| d.remove::<egui::FrameDurations>(id));
}

fn message_body(ui: &mut egui::Ui, m: &ChatMsg, view: &RowView) {
    let show_emotes = view.show_emotes;
    let styled = |t: &str| {
        let r = RichText::new(t);
        if m.deleted {
            r.strikethrough().color(DIM)
        } else {
            r
        }
    };
    if !show_emotes {
        ui.label(styled(&m.text));
        return;
    }
    let size = ui.text_style_height(&egui::TextStyle::Body) * EMOTE_SCALE;
    for part in &m.parts {
        match part {
            Part::Text(t) => {
                ui.label(styled(t));
            }
            Part::Emote { url, alt } => {
                // Wide emotes keep their aspect ratio; height matches the line.
                let uri = animatable_uri(url);
                view.drawn.borrow_mut().push(uri.clone());
                let img = egui::Image::new(uri)
                    .fit_to_exact_size(egui::vec2(size * 4.0, size))
                    .maintain_aspect_ratio(true)
                    .tint(if m.deleted {
                        Color32::from_white_alpha(90)
                    } else {
                        Color32::WHITE
                    });
                ui.add(img).on_hover_text(alt);
            }
        }
    }
}

/// egui only animates images whose URI ends in .gif/.webp, and emote CDNs
/// (Twitch, FFZ, YouTube) often use extensionless URLs. The CDNs ignore the
/// dummy query; egui's loaders then pick GIF/WebP/PNG by sniffing the bytes,
/// so static images still load normally.
pub fn animatable_uri(url: &str) -> String {
    if url.ends_with(".gif") || url.ends_with(".webp") {
        return url.to_string();
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}anim=.gif")
}

fn open_path(path: &std::path::Path) {
    #[cfg(windows)]
    let prog = "explorer";
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let prog = "xdg-open";
    let _ = std::process::Command::new(prog).arg(path).spawn();
}

impl eframe::App for ModApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sweep_emotes(ctx);
        self.drain_events();
        self.drain_mod_events();
        while let Ok((target, result)) = self.discover_rx.try_recv() {
            self.on_discovered(target, result, ctx);
        }
        if !self.channels.is_empty() {
            self.scan_channels(ctx, false);
            // Wake up for the next rescan even if chat is quiet.
            ctx.request_repaint_after(Duration::from_secs(5));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("bottom").show(ui, |ui| self.bottom_bar(ui));
        egui::Panel::right("flagged")
            .resizable(true)
            .default_size(420.0)
            .min_size(260.0)
            .show(ui, |ui| self.flagged_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.chat_panel(ui));

        if self.show_settings {
            self.settings_window(&ctx);
        }
        if self.confirm_quit {
            self.quit_dialog(&ctx);
        }
        if self.confirm_delete.is_some() {
            self.delete_dialog(&ctx);
        }
        if self.yt_menu.is_some() {
            self.yt_menu_window(&ctx);
        }
        if self.confirm_action.is_some() {
            self.action_dialog(&ctx);
        }
        if self.punish.is_some() {
            self.punish_dialog(&ctx);
        }
        if self.show_lists {
            self.lists_window(&ctx);
        } else if self.editing_list.is_some() {
            self.list_editor(&ctx);
        }
        self.persist_config();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.disconnect_all();
        self.paths.save_config(&self.cfg);
    }
}
