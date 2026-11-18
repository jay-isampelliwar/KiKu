use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// All settings for the daemon. Any field missing from the file uses its default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub whisper_model_path: PathBuf,
    pub pyannote_model_path: PathBuf,
    pub python_path: PathBuf,
    pub worker_script: PathBuf,
    pub meetings_dir: PathBuf,
    pub db_path: PathBuf,
    pub log_dir: PathBuf,
    pub mic_device: String,
    pub system_device: String,
    pub llm_model: String,
    pub meeting_apps: Vec<String>,
    pub ignore_apps: Vec<String>,
    pub chunk_seconds: u32,
    pub start_confirm_seconds: u32,
    pub start_window_seconds: u32,
    pub start_voice_seconds: u32,
    pub app_release_seconds: u32,
    pub silence_stop_seconds: u32,
    pub mic_silence_db: f32,
    pub system_silence_db: f32,
    /// Open a Terminal window with the notes when a meeting is summarized.
    pub popup_when_done: bool,
    /// Where this config was loaded from. Not read from the file.
    #[serde(skip)]
    pub config_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            whisper_model_path: "/Volumes/T7/Local-Model/whisper/faster-whisper-medium".into(),
            pyannote_model_path: "/Volumes/T7/Local-Model/pyannote/speaker-diarization-community-1"
                .into(),
            python_path: "worker/.venv/bin/python".into(),
            worker_script: "worker/src/__main__.py".into(),
            meetings_dir: "~/Meetings".into(),
            db_path: "~/Library/Application Support/Preeti/meetings.db".into(),
            log_dir: "~/Library/Logs/Preeti".into(),
            mic_device: "CEVCECM".into(),
            system_device: "BlackHole 2ch".into(),
            llm_model: "gpt-5-mini".into(),
            meeting_apps: [
                "zoom.us",
                "Google Chrome",
                "Brave Browser",
                "Slack",
                "WhatsApp",
                "Telegram",
                "Microsoft Teams",
                "FaceTime",
            ]
            .map(String::from)
            .to_vec(),
            ignore_apps: ["Visual Studio Code", "ChatGPT", "Codex", "Claude"]
                .map(String::from)
                .to_vec(),
            chunk_seconds: 30,
            start_confirm_seconds: 15,
            start_window_seconds: 30,
            start_voice_seconds: 3,
            app_release_seconds: 10,
            silence_stop_seconds: 90,
            mic_silence_db: -35.0,
            system_silence_db: -50.0,
            popup_when_done: true,
            config_path: PathBuf::new(),
        }
    }
}

impl Config {
    /// Settings for the meeting detector.
    pub fn detector_settings(&self) -> crate::detect::Settings {
        crate::detect::Settings {
            start_confirm_seconds: self.start_confirm_seconds,
            start_window_seconds: self.start_window_seconds,
            start_voice_seconds: self.start_voice_seconds,
            app_release_seconds: self.app_release_seconds,
            silence_stop_seconds: self.silence_stop_seconds,
            mic_silence_db: self.mic_silence_db,
            system_silence_db: self.system_silence_db,
        }
    }

    /// While this file exists, no meeting is recorded.
    pub fn pause_file(&self) -> PathBuf {
        self.db_path.with_file_name("paused")
    }

    /// Read the TOML file if it exists, otherwise use defaults. Then expand `~` in paths.
    pub fn load(path: &Path) -> Result<Self> {
        let mut config: Config = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?
        } else {
            Config::default()
        };

        for field in [
            &mut config.whisper_model_path,
            &mut config.pyannote_model_path,
            &mut config.python_path,
            &mut config.worker_script,
            &mut config.meetings_dir,
            &mut config.db_path,
            &mut config.log_dir,
        ] {
            *field = expand_home(field);
        }

        config.config_path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());

        Ok(config)
    }
}

/// Turn a leading `~/` into the user's home folder.
fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}
