//! Decides when a meeting starts and ends. Pure logic: one `tick` per second, no I/O.

mod mic_users;

pub use mic_users::{MicUser, mic_users};

/// Timings and thresholds for the state machine, in seconds and dB.
#[derive(Debug, Clone)]
pub struct Settings {
    pub start_confirm_seconds: u32,
    pub start_window_seconds: u32,
    pub start_voice_seconds: u32,
    pub app_release_seconds: u32,
    pub silence_stop_seconds: u32,
    pub mic_silence_db: f32,
    pub system_silence_db: f32,
}

/// What the detector sees in one second.
#[derive(Debug, Clone, Copy)]
pub struct Tick {
    /// An allowed meeting app is using the mic.
    pub meeting_app_on_mic: bool,
    /// Loudest 100 ms in the last second. -100 when not capturing.
    pub mic_db: f32,
    pub system_db: f32,
    pub paused: bool,
}

/// What the caller must do after a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Start capturing, but this is not a meeting yet.
    StartCapture,
    /// It is a real call. Save it as a meeting.
    ConfirmMeeting,
    /// It was not a call (voice note, dictation). Stop and delete the audio.
    DiscardCapture,
    /// The meeting is over. Stop and save.
    EndMeeting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Starting { elapsed: u32, voice: u32 },
    Recording { app_gone: u32, silent: u32 },
    /// Wait until the meeting app lets go of the mic before looking again.
    Ignoring,
}

pub struct Detector {
    settings: Settings,
    state: State,
}

impl Detector {
    pub fn new(settings: Settings) -> Self {
        Self { settings, state: State::Idle }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn tick(&mut self, t: Tick) -> Action {
        let s = &self.settings;
        let (next, action) = match self.state {
            State::Idle if t.paused => (State::Idle, Action::Nothing),
            State::Idle if t.meeting_app_on_mic => {
                (State::Starting { elapsed: 0, voice: 0 }, Action::StartCapture)
            }
            State::Idle => (State::Idle, Action::Nothing),

            State::Starting { .. } if t.paused || !t.meeting_app_on_mic => {
                (State::Idle, Action::DiscardCapture)
            }
            State::Starting { elapsed, voice } => {
                let elapsed = elapsed + 1;
                let voice = voice + u32::from(t.system_db > s.system_silence_db);

                if elapsed >= s.start_confirm_seconds && voice >= s.start_voice_seconds {
                    (State::Recording { app_gone: 0, silent: 0 }, Action::ConfirmMeeting)
                } else if elapsed >= s.start_window_seconds {
                    (State::Ignoring, Action::DiscardCapture)
                } else {
                    (State::Starting { elapsed, voice }, Action::Nothing)
                }
            }

            State::Recording { .. } if t.paused => (State::Idle, Action::EndMeeting),
            State::Recording { app_gone, silent } => {
                let app_gone = if t.meeting_app_on_mic { 0 } else { app_gone + 1 };
                let quiet = t.mic_db <= s.mic_silence_db && t.system_db <= s.system_silence_db;
                let silent = if quiet { silent + 1 } else { 0 };

                if app_gone >= s.app_release_seconds || silent >= s.silence_stop_seconds {
                    (State::Ignoring, Action::EndMeeting)
                } else {
                    (State::Recording { app_gone, silent }, Action::Nothing)
                }
            }

            State::Ignoring if !t.meeting_app_on_mic => (State::Idle, Action::Nothing),
            State::Ignoring => (State::Ignoring, Action::Nothing),
        };

        self.state = next;
        action
    }
}

/// True if any mic user is an allowed meeting app and not on the ignore list.
pub fn meeting_app_on_mic(users: &[MicUser], allowed: &[String], ignored: &[String]) -> Option<String> {
    let matches = |list: &[String], app: &str| list.iter().any(|a| a.eq_ignore_ascii_case(app));

    users
        .iter()
        .find(|u| matches(allowed, &u.app) && !matches(ignored, &u.app))
        .map(|u| u.app.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SILENT: f32 = -100.0;
    const ROOM: f32 = -45.0;
    const VOICE: f32 = -20.0;

    fn detector() -> Detector {
        Detector::new(Settings {
            start_confirm_seconds: 15,
            start_window_seconds: 30,
            start_voice_seconds: 3,
            app_release_seconds: 10,
            silence_stop_seconds: 90,
            mic_silence_db: -35.0,
            system_silence_db: -50.0,
        })
    }

    fn tick(app: bool, mic: f32, system: f32) -> Tick {
        Tick { meeting_app_on_mic: app, mic_db: mic, system_db: system, paused: false }
    }

    /// Run `n` identical ticks and return every non-Nothing action.
    fn run(d: &mut Detector, n: u32, t: Tick) -> Vec<Action> {
        (0..n).map(|_| d.tick(t)).filter(|a| *a != Action::Nothing).collect()
    }

    #[test]
    fn real_call_is_confirmed_after_15_seconds() {
        let mut d = detector();
        assert_eq!(d.tick(tick(true, ROOM, SILENT)), Action::StartCapture);
        assert_eq!(run(&mut d, 14, tick(true, VOICE, VOICE)), vec![]);
        assert_eq!(d.tick(tick(true, VOICE, VOICE)), Action::ConfirmMeeting);
    }

    #[test]
    fn voice_note_is_discarded_and_not_retried() {
        let mut d = detector();
        d.tick(tick(true, VOICE, SILENT));
        assert_eq!(run(&mut d, 30, tick(true, VOICE, SILENT)), vec![Action::DiscardCapture]);
        assert_eq!(d.state(), State::Ignoring);
        assert_eq!(run(&mut d, 60, tick(true, VOICE, SILENT)), vec![]);
        d.tick(tick(false, ROOM, SILENT));
        assert_eq!(d.state(), State::Idle);
    }

    #[test]
    fn short_voice_note_released_early_is_discarded() {
        let mut d = detector();
        d.tick(tick(true, VOICE, SILENT));
        assert_eq!(run(&mut d, 5, tick(true, VOICE, SILENT)), vec![]);
        assert_eq!(d.tick(tick(false, ROOM, SILENT)), Action::DiscardCapture);
    }

    #[test]
    fn meeting_ends_when_app_releases_mic() {
        let mut d = detector();
        d.tick(tick(true, VOICE, VOICE));
        run(&mut d, 15, tick(true, VOICE, VOICE));
        assert_eq!(run(&mut d, 9, tick(false, VOICE, VOICE)), vec![]);
        assert_eq!(d.tick(tick(false, VOICE, VOICE)), Action::EndMeeting);
    }

    #[test]
    fn meeting_ends_after_90_seconds_of_silence() {
        let mut d = detector();
        d.tick(tick(true, VOICE, VOICE));
        run(&mut d, 15, tick(true, VOICE, VOICE));
        assert_eq!(run(&mut d, 89, tick(true, ROOM, SILENT)), vec![]);
        assert_eq!(d.tick(tick(true, ROOM, SILENT)), Action::EndMeeting);
    }

    #[test]
    fn sound_during_silence_resets_the_count() {
        let mut d = detector();
        d.tick(tick(true, VOICE, VOICE));
        run(&mut d, 15, tick(true, VOICE, VOICE));
        run(&mut d, 80, tick(true, ROOM, SILENT));
        d.tick(tick(true, ROOM, VOICE));
        assert_eq!(run(&mut d, 80, tick(true, ROOM, SILENT)), vec![]);
    }

    #[test]
    fn pause_ends_meeting_and_blocks_new_ones() {
        let mut d = detector();
        d.tick(tick(true, VOICE, VOICE));
        run(&mut d, 15, tick(true, VOICE, VOICE));
        let paused = Tick { paused: true, ..tick(true, VOICE, VOICE) };
        assert_eq!(d.tick(paused), Action::EndMeeting);
        assert_eq!(run(&mut d, 30, paused), vec![]);
    }

    #[test]
    fn only_allowed_apps_count() {
        let users = |app: &str| vec![MicUser { pid: 1, app: app.into() }];
        let allowed = vec!["zoom.us".to_string(), "Google Chrome".to_string()];
        let ignored = vec!["Visual Studio Code".to_string()];

        assert!(meeting_app_on_mic(&users("zoom.us"), &allowed, &ignored).is_some());
        assert!(meeting_app_on_mic(&users("google chrome"), &allowed, &ignored).is_some());
        assert!(meeting_app_on_mic(&users("Visual Studio Code"), &allowed, &ignored).is_none());
        assert!(meeting_app_on_mic(&users("ChatGPT"), &allowed, &ignored).is_none());
    }
}
