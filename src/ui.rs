use gstreamer as gst;
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    text::Line,
    widgets::Paragraph,
};
use std::{error::Error, time::Duration};

use crate::pipeline::{SessionEvent, StreamSession};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlaybackState {
    Lobby,
    Playing,
    Paused,
}

impl PlaybackState {
    fn label(self) -> &'static str {
        match self {
            Self::Lobby => "LOBBY",
            Self::Playing => "PLAYING",
            Self::Paused => "PAUSED",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Start,
    TogglePause,
    Seek(i64),
    Quit,
}

pub(crate) struct Status<'a> {
    pub(crate) title: &'a str,
    pub(crate) state: PlaybackState,
    pub(crate) position: gst::ClockTime,
    pub(crate) duration: gst::ClockTime,
}

pub(crate) fn command_for_key(state: PlaybackState, key: KeyCode) -> Option<Command> {
    match (state, key) {
        (_, KeyCode::Char('q')) => Some(Command::Quit),
        (PlaybackState::Lobby, KeyCode::Enter) => Some(Command::Start),
        (PlaybackState::Playing | PlaybackState::Paused, KeyCode::Char(' ')) => {
            Some(Command::TogglePause)
        }
        (PlaybackState::Playing | PlaybackState::Paused, KeyCode::Left) => Some(Command::Seek(-30)),
        (PlaybackState::Playing | PlaybackState::Paused, KeyCode::Right) => Some(Command::Seek(30)),
        _ => None,
    }
}

pub(crate) fn format_time(time: gst::ClockTime) -> String {
    let seconds = time.seconds();
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub(crate) fn render(frame: &mut Frame<'_>, status: &Status<'_>) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "{}  {}  {} / {}",
                status.title,
                status.state.label(),
                format_time(status.position),
                format_time(status.duration)
            )),
            Line::default(),
            Line::from("Enter Start · Space Pause/Resume · ←/→ ±30s · q Quit"),
        ]),
        frame.area(),
    );
}

pub(crate) fn run(
    terminal: &mut DefaultTerminal,
    session: &mut StreamSession,
) -> Result<(), Box<dyn Error>> {
    loop {
        if matches!(session.poll()?, SessionEvent::Finished) {
            return Ok(());
        }
        terminal.draw(|frame| {
            render(
                frame,
                &Status {
                    title: session.title(),
                    state: session.state(),
                    position: session.position(),
                    duration: session.duration(),
                },
            );
        })?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        match command_for_key(session.state(), key.code) {
            Some(Command::Start) => session.start()?,
            Some(Command::TogglePause) => session.toggle_pause()?,
            Some(Command::Seek(seconds)) => session.seek_by(seconds)?,
            Some(Command::Quit) => return Ok(()),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyCode};

    #[test]
    fn maps_only_valid_keys_for_each_playback_state() {
        assert_eq!(
            command_for_key(PlaybackState::Lobby, KeyCode::Enter),
            Some(Command::Start)
        );
        assert_eq!(
            command_for_key(PlaybackState::Lobby, KeyCode::Char('q')),
            Some(Command::Quit)
        );
        assert_eq!(
            command_for_key(PlaybackState::Lobby, KeyCode::Char(' ')),
            None
        );
        assert_eq!(command_for_key(PlaybackState::Lobby, KeyCode::Left), None);
        assert_eq!(
            command_for_key(PlaybackState::Playing, KeyCode::Char(' ')),
            Some(Command::TogglePause)
        );
        assert_eq!(
            command_for_key(PlaybackState::Paused, KeyCode::Char(' ')),
            Some(Command::TogglePause)
        );
        assert_eq!(
            command_for_key(PlaybackState::Playing, KeyCode::Left),
            Some(Command::Seek(-30))
        );
        assert_eq!(
            command_for_key(PlaybackState::Paused, KeyCode::Right),
            Some(Command::Seek(30))
        );
        assert_eq!(
            command_for_key(PlaybackState::Playing, KeyCode::Char('x')),
            None
        );
    }

    #[test]
    fn formats_unbounded_hours() {
        assert_eq!(format_time(gst::ClockTime::ZERO), "00:00:00");
        assert_eq!(format_time(gst::ClockTime::from_seconds(3_723)), "01:02:03");
        assert_eq!(
            format_time(gst::ClockTime::from_seconds(442_800)),
            "123:00:00"
        );
    }

    #[test]
    fn renders_compact_status_and_help() {
        let mut terminal = Terminal::new(TestBackend::new(80, 4)).unwrap();
        let status = Status {
            title: "Passenger",
            state: PlaybackState::Playing,
            position: gst::ClockTime::from_seconds(2_538),
            duration: gst::ClockTime::from_seconds(6_423),
        };

        terminal.draw(|frame| render(frame, &status)).unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Passenger  PLAYING  00:42:18 / 01:47:03"));
        assert!(text.contains("Enter Start"));
        assert!(text.contains("Space Pause/Resume"));
        assert!(text.contains("q Quit"));
    }
}
