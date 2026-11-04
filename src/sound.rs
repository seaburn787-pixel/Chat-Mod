//! Flag alert sound. A built-in ping is synthesized at startup so it plays the
//! same everywhere, regardless of the OS sound scheme.

use std::sync::OnceLock;

/// Ping tone, in Hz, and length. Tweak to taste.
const PING_FREQS: [f32; 2] = [880.0, 1320.0];
const PING_NOTE_SECS: f32 = 0.09;
const PING_VOLUME: f32 = 0.5;
const SAMPLE_RATE: u32 = 22_050;

/// Plays the alert on a background thread so the UI never blocks.
pub fn play(custom_file: &str) {
    let file = custom_file.trim().to_string();
    std::thread::spawn(move || {
        play_blocking(&file);
    });
}

fn ping_wav() -> &'static [u8] {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    WAV.get_or_init(build_ping)
}

/// Two short rising notes with a fade-out on each, as a 16-bit mono WAV.
fn build_ping() -> Vec<u8> {
    let per_note = (SAMPLE_RATE as f32 * PING_NOTE_SECS) as usize;
    let mut samples = Vec::with_capacity(per_note * PING_FREQS.len());
    for freq in PING_FREQS {
        for i in 0..per_note {
            let t = i as f32 / SAMPLE_RATE as f32;
            let progress = i as f32 / per_note as f32;
            let attack = (progress * 40.0).min(1.0); // avoids a click at note start
            let envelope = attack * (1.0 - progress).powi(2);
            let v = (t * freq * std::f32::consts::TAU).sin() * envelope * PING_VOLUME;
            samples.push((v * i16::MAX as f32) as i16);
        }
    }

    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

#[cfg(windows)]
fn play_blocking(file: &str) -> bool {
    use windows_sys::Win32::Media::Audio::{
        PlaySoundW, SND_FILENAME, SND_MEMORY, SND_NODEFAULT, SND_SYNC,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows_sys::Win32::UI::WindowsAndMessaging::MB_OK;

    unsafe {
        if !file.is_empty() {
            let wide: Vec<u16> = file.encode_utf16().chain(Some(0)).collect();
            if PlaySoundW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                SND_FILENAME | SND_NODEFAULT | SND_SYNC,
            ) != 0
            {
                return true;
            }
        }
        let wav = ping_wav();
        if PlaySoundW(
            wav.as_ptr() as *const u16,
            std::ptr::null_mut(),
            SND_MEMORY | SND_NODEFAULT | SND_SYNC,
        ) != 0
        {
            return true;
        }
        MessageBeep(MB_OK) != 0
    }
}

#[cfg(not(windows))]
fn play_blocking(file: &str) -> bool {
    use std::process::{Command, Stdio};

    let run = |prog: &str, args: &[&str]| {
        Command::new(prog)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let play_file =
        |f: &str| run("paplay", &[f]) || run("pw-play", &[f]) || run("aplay", &["-q", f]);

    if !file.is_empty() && play_file(file) {
        return true;
    }
    let ping = std::env::temp_dir().join("chat-mod-ping.wav");
    if !ping.exists() {
        let _ = std::fs::write(&ping, ping_wav());
    }
    if let Some(p) = ping.to_str()
        && play_file(p)
    {
        return true;
    }
    if run("canberra-gtk-play", &["-i", "message-new-instant"]) {
        return true;
    }
    print!("\x07");
    false
}

#[cfg(test)]
mod tests {
    #[test]
    fn ping_is_valid_wav() {
        let w = super::build_ping();
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        let data_len = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
        assert_eq!(w.len(), 44 + data_len);
    }
}
