// SPDX-License-Identifier: GPL-3.0-or-later
//! Text to speech at runtime over the Wyoming protocol (as Home Assistant does it), e.g. the
//! container rhasspy/wyoming-piper (deploy/examples/compose.tts.yml). PBX_TTS=wyoming://host:port,
//! voices per language PBX_TTS_VOICE_DE / PBX_TTS_VOICE_EN.
//!
//! Results are cached as 8 kHz WAV under /media/tts/<sha256>.wav (Asterisk reads /media), keyed by
//! voice and text; Asterisk plays them by absolute path without extension.
//!
//! Wyoming: every event is a JSON header line (`type`, optional `data`, `data_length`,
//! `payload_length`), then `data_length` bytes of JSON data and `payload_length` bytes of payload.
//! A `synthesize` request is answered with `audio-start`, `audio-chunk`s (PCM) and `audio-stop`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};

const DIR: &str = "/media/tts";
/// longest text spoken in one piece (scripts split their output into several pieces)
pub const MAX_TEXT: usize = 1000;

#[derive(Clone, Debug)]
pub struct TtsConfig {
    /// host:port of the Wyoming server
    pub addr: String,
    pub voice_de: String,
    pub voice_en: String,
}

impl TtsConfig {
    /// PBX_TTS (empty: no runtime TTS)
    pub fn from_env() -> Result<Option<TtsConfig>> {
        let v = std::env::var("PBX_TTS").unwrap_or_default();
        let v = v.trim();
        if v.is_empty() || v == "off" {
            return Ok(None);
        }
        let addr = v.strip_prefix("wyoming://").context("PBX_TTS must be wyoming://host:port")?.trim_end_matches('/').to_string();
        anyhow::ensure!(addr.contains(':'), "PBX_TTS: port missing (wyoming://host:10200)");
        let var = |k: &str, d: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| d.to_string());
        Ok(Some(TtsConfig {
            addr,
            voice_de: var("PBX_TTS_VOICE_DE", "de_DE-thorsten-medium"),
            voice_en: var("PBX_TTS_VOICE_EN", "en_US-lessac-medium"),
        }))
    }

    fn voice(&self, lang: &str) -> &str {
        if lang == "de" { &self.voice_de } else { &self.voice_en }
    }
}

/// Speaks `text` in `lang` ("de", else English) and returns the path for Playback (no extension).
pub async fn speak(cfg: &TtsConfig, lang: &str, text: &str) -> Result<String> {
    let text: String = text.chars().filter(|c| !c.is_control() || *c == '\n').take(MAX_TEXT).collect();
    let text = text.trim();
    if text.is_empty() {
        bail!("nothing to say");
    }
    let voice = cfg.voice(lang);
    let key = hex::encode(Sha256::digest(format!("{voice}\n{text}").as_bytes()));
    let base = format!("{DIR}/{key}");
    let wav = format!("{base}.wav");
    if tokio::fs::metadata(&wav).await.is_ok() {
        // keep recently used files out of the cleanup
        let _ = filetime_touch(&wav).await;
        return Ok(base);
    }
    let (pcm, rate, width, channels) = timeout(Duration::from_secs(20), synthesize(&cfg.addr, voice, text)).await.context("TTS timeout")??;
    tokio::fs::create_dir_all(DIR).await?;
    let tmp = format!("{base}.{}.tmp.wav", std::process::id());
    to_wav8k(&pcm, rate, width, channels, &tmp).await?;
    tokio::fs::rename(&tmp, &wav).await?;
    Ok(base)
}

async fn filetime_touch(path: &str) -> std::io::Result<()> {
    let f = std::fs::File::options().append(true).open(path)?;
    f.set_modified(std::time::SystemTime::now())
}

/// One synthesize round trip: (PCM, rate, sample width in bytes, channels).
async fn synthesize(addr: &str, voice: &str, text: &str) -> Result<(Vec<u8>, u32, u32, u32)> {
    let s = timeout(Duration::from_secs(3), TcpStream::connect(addr)).await.context("TTS server not reachable")??;
    let (r, mut w) = s.into_split();
    let mut r = BufReader::new(r);
    let req = serde_json::json!({"type": "synthesize", "data": {"text": text, "voice": {"name": voice}}});
    w.write_all(format!("{req}\n").as_bytes()).await?;
    let (mut rate, mut width, mut channels) = (22050, 2, 1);
    let mut pcm = Vec::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await? == 0 {
            bail!("TTS server closed the connection");
        }
        let mut head: serde_json::Value = serde_json::from_str(line.trim()).context("TTS: bad event header")?;
        let data_len = head.get("data_length").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let payload_len = head.get("payload_length").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        anyhow::ensure!(data_len <= 1 << 20 && payload_len <= 16 << 20, "TTS: event too large");
        if data_len > 0 {
            let mut d = vec![0; data_len];
            r.read_exact(&mut d).await?;
            head["data"] = serde_json::from_slice(&d).context("TTS: bad event data")?;
        }
        let mut payload = vec![0; payload_len];
        if payload_len > 0 {
            r.read_exact(&mut payload).await?;
        }
        let data = &head["data"];
        let num = |k: &str, d: u32| data.get(k).and_then(|v| v.as_u64()).map(|v| v as u32).unwrap_or(d);
        match head["type"].as_str().unwrap_or("") {
            "audio-start" | "audio-chunk" => {
                rate = num("rate", rate);
                width = num("width", width);
                channels = num("channels", channels);
                pcm.extend_from_slice(&payload);
                anyhow::ensure!(pcm.len() <= 64 << 20, "TTS: audio too long");
            }
            "audio-stop" => break,
            "error" => bail!("TTS error: {}", data.get("text").and_then(|t| t.as_str()).unwrap_or("?")),
            _ => {}
        }
    }
    anyhow::ensure!(!pcm.is_empty(), "TTS returned no audio");
    Ok((pcm, rate, width, channels))
}

/// Raw PCM → 8 kHz mono 16-bit WAV (what Asterisk's wav format expects).
async fn to_wav8k(pcm: &[u8], rate: u32, width: u32, channels: u32, out: &str) -> Result<()> {
    let fmt = match width {
        1 => "u8",
        2 => "s16le",
        4 => "s32le",
        _ => bail!("TTS: unsupported sample width {width}"),
    };
    let mut child = tokio::process::Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-f", fmt, "-ar", &rate.to_string(), "-ac", &channels.to_string(), "-i", "-"])
        .args(["-ar", "8000", "-ac", "1", "-c:a", "pcm_s16le", out])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("ffmpeg")?;
    let mut stdin = child.stdin.take().context("ffmpeg stdin")?;
    stdin.write_all(pcm).await?;
    drop(stdin);
    let st = child.wait().await?;
    anyhow::ensure!(st.success(), "ffmpeg failed");
    Ok(())
}

/// Housekeeping: drop cached speech not used for `days` days.
pub async fn cleanup(days: u64) -> Result<usize> {
    let mut n = 0;
    let Ok(mut rd) = tokio::fs::read_dir(DIR).await else { return Ok(0) };
    let limit = std::time::SystemTime::now() - Duration::from_secs(days * 86400);
    while let Some(e) = rd.next_entry().await? {
        let old = e.metadata().await.ok().and_then(|m| m.modified().ok()).is_some_and(|t| t < limit);
        if old && tokio::fs::remove_file(e.path()).await.is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    /// Against a real server: PBX_TEST_TTS=127.0.0.1:10200 cargo test tts -- --ignored
    #[tokio::test]
    #[ignore]
    async fn wyoming_round_trip() {
        let addr = std::env::var("PBX_TEST_TTS").expect("PBX_TEST_TTS=host:port");
        let (pcm, rate, width, ch) = super::synthesize(&addr, "de_DE-thorsten-medium", "Das Lab ist offen.").await.unwrap();
        assert!(pcm.len() > 10_000 && rate > 8000 && width == 2 && ch == 1, "{} bytes {rate} Hz", pcm.len());
        let out = std::env::temp_dir().join("pbx-tts-test.wav");
        super::to_wav8k(&pcm, rate, width, ch, out.to_str().unwrap()).await.unwrap();
        assert!(std::fs::metadata(&out).unwrap().len() > 5_000);
    }
}
