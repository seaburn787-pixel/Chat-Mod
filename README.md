# Chat Mod

A desktop moderation companion for **YouTube and Twitch live chat**. It merges several chats into one window, flags messages that match your word lists, and gives you one-click **delete / timeout / ban**. Every action asks for confirmation first. Nothing is ever done automatically.

Built in Rust with [egui](https://github.com/emilk/egui). Developed and tested on **Windows**. It should build on Linux, but that is untested.

## Features

- **Merged chat.** Add any mix of:
  - a YouTube `@channel`, which pulls in all of its live and upcoming/waiting-room streams and rescans for new ones,
  - a single YouTube video URL,
  - `twitch.tv/name`.

  Messages from different streams are tagged and colour-coded.
- **No API keys needed to read chat.**
  - YouTube is read through the same endpoint youtube.com uses, and you get the full **Live chat** view, not Top chat.
  - Twitch is read through anonymous IRC.
- **Emotes.** Twitch, 7TV, BTTV, FFZ and YouTube emotes, both static and animated.
- **Word-list flagging.**
  - Matching messages turn red, go into a **Flagged** panel, and play a sound.
  - Plain rules see through common evasion: `sc4m`, `$c@m`, `s c a m`, `s.c.a.m`, `scaaam`, accented and Cyrillic/Greek lookalike letters, and fullwidth or math-styled Unicode.
  - **Spelled-out words** are caught even when someone types one or two letters per message (`x` / `y` / `z`).
  - **Usernames** can be checked too, so a list word hidden anywhere in a name gets flagged. There's an allow list for innocent names, e.g. "Cassandra".
- **Moderation.** 🗑 delete, ⏱ timeout and 🔨 ban appear on every message and in the Flagged panel.
  - On YouTube the ☰ menu offers the same moderation options the website shows you.
  - Messages deleted by anyone on the platform side are struck through in Chat Mod as well.

## Getting started

### 1. Install Rust (one time)

1. Go to **[rustup.rs](https://rustup.rs)** and download `rustup-init.exe`.
2. Run it, and press **Enter** to accept the defaults. If it offers to install the Visual Studio C++ build tools, say yes, because Rust needs them.
3. Close and reopen any terminal windows when it's done.

### 2. Download and build Chat Mod

Open **PowerShell** or **Command Prompt** and run:

```bash
git clone https://github.com/seaburn787-pixel/Chat-Mod.git
cd Chat-Mod
cargo build --release
```

No git? On this GitHub page, click **Code → Download ZIP**, unzip it, open a terminal in that folder, and run just `cargo build --release`.

The first build downloads its libraries and takes a few minutes. Later builds are much faster.

### 3. Run it

Open `target\release\chat-mod.exe`. You can make a desktop shortcut to it.

Paste a YouTube `@channel`, a video link or a `twitch.tv/name` link into the top bar, click **Add**, then **Connect all**.

### Updating later

```bash
cd Chat-Mod
git pull
cargo build --release
```

Your settings and word lists aren't affected. They live in `%APPDATA%\chat-mod`, not in this folder.

## Word lists

Every `.txt` file in the `lists/` folder is a list. Use **Word lists** in the app to switch lists on and off, edit them, create new ones, and test them live.

```
scam              whole word, any case (plus the evasion handling above)
free robux        phrase
hack*             starts with   (*coin = ends with, *bot* = contains)
whats*app         also matches "whats app"
re:t\.me/\S+      regex, case-insensitive, matched against the raw text
user:SomeName     always flag this author (exact name, or user:UC... channel id)
name:johnny       flag anyone whose name CONTAINS this (JohnnyV2, xX_j0hnny_Xx)
# comment
```

Matching is whole-word, so `ass` does **not** flag "class" or "pass". The channel owner is never flagged, and moderators are exempt by default (Settings).

## Moderating

You must be a moderator in the channel. Chat Mod acts **as you**, with the same permissions you have on the website.

### Twitch

Do this once:

1. Go to [dev.twitch.tv/console](https://dev.twitch.tv/console) and click **Register Your Application**:
   - **OAuth Redirect URL:** `http://localhost` (required by the form, but not used)
   - **Category:** Chat Bot
   - **Client Type:** **Public**
2. Copy the **Client ID** into **Settings → Twitch moderation** and click **Log in with Twitch**.
3. Enter the code it shows at [twitch.tv/activate](https://www.twitch.tv/activate).

Chat Mod asks only for permission to delete messages and to ban/timeout users. No client secret is needed.

### YouTube

YouTube moderation uses the same requests the website makes, signed with your browser's login cookie:

1. Open youtube.com, logged in as the account that mods the channel.
2. Press **F12**, go to the **Network** tab, and reload the page.
3. Click the `www.youtube.com` document request, then find **Request Headers → cookie** and copy its value.
   - In Chrome/Edge, tick **Disable cache** first, then reload with **Ctrl+Shift+R**. If a request shows "(from service worker)" it has no cookie header.
4. Paste the value into **Settings → YouTube moderation**.

> **That cookie is your Google login.** It's stored only on your PC, in `youtube_login.json`. **Log out** in Chat Mod deletes it. Never share that file. Logging out of YouTube in your browser also invalidates the copy Chat Mod holds.

## Data folder

`%APPDATA%\chat-mod\config\` on Windows (`~/.config/chat-mod/` on Linux) contains:

| File | What |
|---|---|
| `config.toml` | Settings and your stream list. Contains no secrets. |
| `lists/*.txt` | Word lists. `names_ok.txt` is the username allow list. |
| `flagged.log` | Append-only record of every flag. |
| `twitch_login.json` / `youtube_login.json` | Moderation logins. Created only when you log in, and deleted when you log out. **Private.** |

## Optional: official YouTube API

The source dropdown can switch YouTube reading to the official **Data API v3**. You'll need a key from Google Cloud Console (enable *YouTube Data API v3*, then go to *Credentials* → *API key*). It is slower and costs quota (about 5 units per poll, out of 10,000 per day), so the default no-key reader is recommended.

## Development

```bash
cargo test
cargo clippy --all-targets
cargo fmt
```

| Module | |
|---|---|
| `app.rs` | UI, moderation dialogs |
| `filter.rs` | Word-list rules and evasion folding |
| `spelled.rs` | Words spelled across messages |
| `source.rs`, `source/` | YouTube (InnerTube + API), Twitch IRC, channel stream discovery |
| `moderation.rs` | Twitch login (device flow) and Helix delete/ban |
| `yt_mod.rs` | YouTube ☰-menu actions |
| `config.rs`, `model.rs`, `sound.rs` | Settings, shared types, flag sound |

**Linux (untested).** Install the build dependencies, then build normally:

```bash
sudo apt install build-essential pkg-config libx11-dev libxkbcommon-dev libgl1-mesa-dev \
                 libxcursor-dev libxrandr-dev libxi-dev libwayland-dev
```

Sound uses `paplay`, `pw-play` or `aplay`.

## Caveats

YouTube chat reading and YouTube moderation use YouTube's internal web endpoints, not a public API, so a YouTube website change can break them until they're updated. Use the moderation features within the platforms' rules.

## License

[MIT](LICENSE)
