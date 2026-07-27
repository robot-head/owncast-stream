use gstreamer as gst;
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use std::{error::Error, time::Duration};

use crate::pipeline::{AudioLevels, SessionEvent, StreamSession};

const AMBER: Color = Color::Rgb(255, 201, 40);
const BLACK: Color = Color::Black;

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
    Gain(i8),
    Quit,
}

pub(crate) struct Status<'a> {
    pub(crate) title: &'a str,
    pub(crate) state: PlaybackState,
    pub(crate) position: gst::ClockTime,
    pub(crate) duration: gst::ClockTime,
    pub(crate) gain_db: f64,
    pub(crate) levels: AudioLevels,
}

pub(crate) fn command_for_key(state: PlaybackState, key: KeyCode) -> Option<Command> {
    match (state, key) {
        (_, KeyCode::Char('q')) => Some(Command::Quit),
        (_, KeyCode::Up) => Some(Command::Gain(1)),
        (_, KeyCode::Down) => Some(Command::Gain(-1)),
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

fn meter_position(db: f64, width: usize) -> usize {
    ((((db.clamp(-60.0, 0.0) + 60.0) / 60.0) * width as f64).round() as usize).min(width)
}

fn meter_bar(width: usize, peak: f64, decay: f64) -> String {
    if width == 0 {
        return String::new();
    }
    let mut cells = vec!['·'; width];
    for cell in cells.iter_mut().take(meter_position(peak, width)) {
        *cell = '█';
    }
    cells[meter_position(decay, width).min(width - 1)] = '┃';
    cells.into_iter().collect()
}

fn panel(title: &'static str) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(AMBER))
        .style(Style::default().fg(AMBER).bg(BLACK))
}

fn render_meter(frame: &mut Frame<'_>, area: Rect, status: &Status<'_>) {
    let width = usize::from(area.width.saturating_sub(14));
    let meter = |channel: usize, label: &str| {
        Line::from(vec![
            Span::raw(format!("{label} ")),
            Span::raw(meter_bar(
                width,
                status.levels.peak[channel],
                status.levels.decay[channel],
            )),
            Span::raw(format!(" {:>5.1} dB", status.levels.peak[channel])),
        ])
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("-60 dB".to_owned() + &" ".repeat(width.saturating_sub(5)) + "0 dB"),
            meter(0, "L"),
            meter(1, "R"),
        ])
        .block(panel("PROGRAM AUDIO / 3 SEC PEAK HOLD")),
        area,
    );
}

fn render_controls(frame: &mut Frame<'_>, area: Rect, wide: bool) {
    let controls = [
        ("ENTER", "START"),
        ("SPACE", "PAUSE / RESUME"),
        ("← / →", "JUMP 30 SEC"),
        ("↑ / ↓", "GAIN 1 dB"),
        ("Q", "EXIT SHOW"),
    ];
    let render_row = |frame: &mut Frame<'_>, area: Rect, controls: &[(&str, &str)]| {
        let chunks = Layout::horizontal(vec![
            Constraint::Ratio(1, controls.len() as u32);
            controls.len()
        ])
        .split(area);
        for ((key, action), chunk) in controls.iter().zip(chunks.iter()) {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        *key,
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(*action),
                ])
                .alignment(Alignment::Center)
                .block(panel("CONTROL")),
                *chunk,
            );
        }
    };

    if wide {
        render_row(frame, area, &controls);
    } else {
        let rows = Layout::vertical([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(area);
        render_row(frame, rows[0], &controls[..3]);
        render_row(frame, rows[1], &controls[3..]);
    }
}

fn render_main(frame: &mut Frame<'_>, area: Rect, status: &Status<'_>, wide: bool) {
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(6),
        Constraint::Length(3),
        Constraint::Min(7),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "OWNCAST",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("SHOW LOCAL - AUTO MODE"),
        ]),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(format!("TITLE: {}", status.title.to_uppercase()))
            .block(panel("SHOW INFORMATION")),
        rows[1],
    );
    let inverted = Style::default().fg(BLACK).bg(AMBER);
    frame.render_widget(
        Paragraph::new(format!(
            " STATUS: {}     PRESETS COMPLETE",
            status.state.label()
        ))
        .style(inverted),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(format!(
            " SHOW TIME {} / {}     GAIN {:+.0} dB",
            format_time(status.position),
            format_time(status.duration),
            status.gain_db
        ))
        .style(inverted),
        rows[3],
    );
    render_meter(frame, rows[4], status);
    frame.render_widget(
        Paragraph::new(format!("** {} MODE **", status.state.label()))
            .alignment(Alignment::Center)
            .style(Style::default().add_modifier(Modifier::BOLD))
            .block(panel("").border_type(BorderType::Double)),
        rows[5],
    );
    render_controls(frame, rows[6], wide);
}

fn render_rail(frame: &mut Frame<'_>, area: Rect, status: &Status<'_>) {
    let rows = Layout::vertical([
        Constraint::Ratio(2, 5),
        Constraint::Ratio(1, 5),
        Constraint::Ratio(2, 5),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("□ STREAM READY"),
            Line::from(format!("□ {}", status.state.label())),
        ])
        .block(panel("SYSTEM")),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(format!("{:+.0} dB", status.gain_db))
            .alignment(Alignment::Center)
            .block(panel("GAIN VALUE")),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("L {:>5.1} dB", status.levels.decay[0])),
            Line::from(format!("R {:>5.1} dB", status.levels.decay[1])),
        ])
        .block(panel("PEAK VALUES")),
        rows[2],
    );
}

pub(crate) fn render(frame: &mut Frame<'_>, status: &Status<'_>) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(AMBER).bg(BLACK)),
        area,
    );
    let wide = area.width >= 90;
    let direction = if wide {
        Direction::Horizontal
    } else {
        Direction::Vertical
    };
    let sections = Layout::default()
        .direction(direction)
        .constraints(if wide {
            [Constraint::Ratio(3, 4), Constraint::Ratio(1, 4)]
        } else {
            [Constraint::Min(23), Constraint::Length(11)]
        })
        .split(area);
    render_main(frame, sections[0], status, wide);
    render_rail(frame, sections[1], status);
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
                    gain_db: session.gain_db(),
                    levels: session.levels(),
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
            Some(Command::Gain(steps)) => session.adjust_gain(steps),
            Some(Command::Quit) => return Ok(()),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::AudioLevels;
    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyCode};

    fn status() -> Status<'static> {
        Status {
            title: "Passenger",
            state: PlaybackState::Playing,
            position: gst::ClockTime::from_seconds(2_538),
            duration: gst::ClockTime::from_seconds(6_423),
            gain_db: 3.0,
            levels: AudioLevels {
                peak: [-4.2, -6.1],
                decay: [-2.8, -3.4],
            },
        }
    }

    #[test]
    fn maps_only_valid_keys_for_each_playback_state() {
        for state in [
            PlaybackState::Lobby,
            PlaybackState::Playing,
            PlaybackState::Paused,
        ] {
            assert_eq!(command_for_key(state, KeyCode::Up), Some(Command::Gain(1)));
            assert_eq!(
                command_for_key(state, KeyCode::Down),
                Some(Command::Gain(-1))
            );
        }
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
    fn meter_bar_places_floor_marker() {
        assert_eq!(meter_bar(10, -60.0, -60.0), "┃·········");
    }

    #[test]
    fn meter_bar_fills_to_ceiling() {
        assert_eq!(meter_bar(10, 0.0, 0.0), "█████████┃");
    }

    #[test]
    fn meter_bar_fills_to_current_peak() {
        let bar = meter_bar(10, -30.0, -6.0);
        assert_eq!(bar.chars().filter(|cell| *cell == '█').count(), 5);
    }

    #[test]
    fn meter_bar_marks_decay_peak() {
        let bar = meter_bar(10, -30.0, -6.0);
        assert_eq!(bar.chars().position(|cell| cell == '┃'), Some(9));
    }

    #[test]
    fn renders_amber_projection_console() {
        let mut terminal = Terminal::new(TestBackend::new(110, 28)).unwrap();
        terminal.draw(|frame| render(frame, &status())).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("OWNCAST"));
        assert!(text.contains("SHOW LOCAL - AUTO MODE"));
        assert!(text.contains("SHOW INFORMATION"));
        assert!(text.contains("TITLE: PASSENGER"));
        assert!(text.contains("STATUS: PLAYING"));
        assert!(text.contains("00:42:18 / 01:47:03"));
        assert!(text.contains("GAIN +3 dB"));
        assert!(text.contains("PROGRAM AUDIO / 3 SEC PEAK HOLD"));
        assert!(text.contains("ENTER"));
        assert!(text.contains("↑ / ↓"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.fg == AMBER)
        );
    }

    #[test]
    fn narrow_console_keeps_core_status_and_controls() {
        let mut terminal = Terminal::new(TestBackend::new(60, 34)).unwrap();
        terminal.draw(|frame| render(frame, &status())).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        for required in [
            "PASSENGER",
            "PLAYING",
            "00:42:18",
            "GAIN +3 dB",
            "L",
            "R",
            "Q",
        ] {
            assert!(text.contains(required), "missing {required}");
        }
    }
}
