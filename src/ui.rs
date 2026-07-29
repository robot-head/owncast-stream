use gstreamer as gst;
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    media::{self, MediaInfo},
    pipeline::{AudioLevels, StreamSession},
};

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
enum Focus {
    Playback,
    Playlist,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Start,
    TogglePause,
    Seek(i64),
    Gain(i8),
    PreviousTrack,
    NextTrack,
    OpenChooser,
    TogglePlaylistFocus,
    Select(i32),
    Move(i32),
    Remove,
    Quit,
}

pub(crate) struct Status<'a> {
    pub(crate) title: &'a str,
    pub(crate) state: PlaybackState,
    pub(crate) position: gst::ClockTime,
    pub(crate) duration: gst::ClockTime,
    pub(crate) gain_db: f64,
    pub(crate) levels: AudioLevels,
    playlist: &'a [MediaInfo],
    active_index: Option<usize>,
    selected_index: usize,
    next_index: usize,
    focus: Focus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileKind {
    Parent,
    Directory,
    File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChooserCommand {
    Select(i32),
    Activate,
    Parent,
    Cancel,
}

#[derive(Debug, Eq, PartialEq)]
struct FileEntry {
    path: PathBuf,
    label: String,
    kind: FileKind,
}

struct FileChooser {
    directory: PathBuf,
    entries: Vec<FileEntry>,
    selected: usize,
    error: Option<String>,
}

impl FileChooser {
    fn open(directory: &Path) -> Result<Self, Box<dyn Error>> {
        let mut chooser = Self {
            directory: directory.to_owned(),
            entries: Vec::new(),
            selected: 0,
            error: None,
        };
        chooser.load(directory)?;
        Ok(chooser)
    }

    fn load(&mut self, directory: &Path) -> Result<(), Box<dyn Error>> {
        let mut directories = Vec::new();
        let mut files = Vec::new();
        for item in fs::read_dir(directory)? {
            let path = item?.path();
            let label = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                directories.push(FileEntry {
                    path,
                    label,
                    kind: FileKind::Directory,
                });
            } else if path.is_file() {
                files.push(FileEntry {
                    path,
                    label,
                    kind: FileKind::File,
                });
            }
        }
        let sort = |left: &FileEntry, right: &FileEntry| {
            left.label
                .to_lowercase()
                .cmp(&right.label.to_lowercase())
                .then_with(|| left.label.cmp(&right.label))
        };
        directories.sort_by(sort);
        files.sort_by(sort);

        let mut entries = Vec::with_capacity(1 + directories.len() + files.len());
        if let Some(parent) = directory.parent() {
            entries.push(FileEntry {
                path: parent.to_owned(),
                label: "..".to_owned(),
                kind: FileKind::Parent,
            });
        }
        entries.extend(directories);
        entries.extend(files);
        self.directory = directory.to_owned();
        self.entries = entries;
        self.selected = 0;
        self.error = None;
        Ok(())
    }

    fn select_by(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        self.selected =
            (self.selected as i32 + delta).clamp(0, self.entries.len() as i32 - 1) as usize;
    }

    fn activate_selected(&mut self) -> Result<Option<PathBuf>, Box<dyn Error>> {
        let Some(entry) = self.entries.get(self.selected) else {
            return Ok(None);
        };
        match entry.kind {
            FileKind::Parent | FileKind::Directory => {
                let path = entry.path.clone();
                self.load(&path)?;
                Ok(None)
            }
            FileKind::File => Ok(Some(entry.path.clone())),
        }
    }

    fn go_to_parent(&mut self) -> Result<(), Box<dyn Error>> {
        let Some(parent) = self.directory.parent().map(Path::to_owned) else {
            return Ok(());
        };
        self.load(&parent)
    }
}

fn chooser_command_for_key(key: event::KeyEvent) -> Option<ChooserCommand> {
    match (key.code, key.modifiers) {
        (KeyCode::Up, _) => Some(ChooserCommand::Select(-1)),
        (KeyCode::Down, _) => Some(ChooserCommand::Select(1)),
        (KeyCode::Enter, _) => Some(ChooserCommand::Activate),
        (KeyCode::Backspace, _) => Some(ChooserCommand::Parent),
        (KeyCode::Esc, _) => Some(ChooserCommand::Cancel),
        _ => None,
    }
}

fn command_for_key(focus: Focus, state: PlaybackState, key: event::KeyEvent) -> Option<Command> {
    match (focus, state, key.code, key.modifiers) {
        (_, _, KeyCode::Char('q'), _) => Some(Command::Quit),
        (_, _, KeyCode::Char('a'), _) => Some(Command::OpenChooser),
        (_, _, KeyCode::Char('p'), _) => Some(Command::PreviousTrack),
        (_, _, KeyCode::Char('n'), _) => Some(Command::NextTrack),
        (Focus::Playlist, _, KeyCode::Up, KeyModifiers::SHIFT) => Some(Command::Move(-1)),
        (Focus::Playlist, _, KeyCode::Down, KeyModifiers::SHIFT) => Some(Command::Move(1)),
        (Focus::Playlist, _, KeyCode::Up, _) => Some(Command::Select(-1)),
        (Focus::Playlist, _, KeyCode::Down, _) => Some(Command::Select(1)),
        (Focus::Playlist, _, KeyCode::Delete | KeyCode::Char('d'), _) => Some(Command::Remove),
        (Focus::Playlist, _, KeyCode::Tab | KeyCode::Esc, _) => Some(Command::TogglePlaylistFocus),
        (Focus::Playback, _, KeyCode::Tab, _) => Some(Command::TogglePlaylistFocus),
        (Focus::Playback, _, KeyCode::Up, _) => Some(Command::Gain(1)),
        (Focus::Playback, _, KeyCode::Down, _) => Some(Command::Gain(-1)),
        (Focus::Playback, PlaybackState::Lobby, KeyCode::Enter, _) => Some(Command::Start),
        (
            Focus::Playback,
            PlaybackState::Playing | PlaybackState::Paused,
            KeyCode::Char(' '),
            _,
        ) => Some(Command::TogglePause),
        (Focus::Playback, PlaybackState::Playing | PlaybackState::Paused, KeyCode::Left, _) => {
            Some(Command::Seek(-30))
        }
        (Focus::Playback, PlaybackState::Playing | PlaybackState::Paused, KeyCode::Right, _) => {
            Some(Command::Seek(30))
        }
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

fn truncate_title(title: &str, max_chars: usize) -> String {
    let length = title.chars().count();
    if length <= max_chars {
        return title.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    title
        .chars()
        .take(max_chars - 1)
        .chain(std::iter::once('…'))
        .collect()
}

fn playlist_window(selected: usize, height: usize, length: usize) -> std::ops::Range<usize> {
    if height == 0 || length == 0 {
        return 0..0;
    }
    let height = height.min(length);
    let start = selected
        .min(length - 1)
        .saturating_add(1)
        .saturating_sub(height)
        .min(length - height);
    start..start + height
}

fn render_playlist(frame: &mut Frame<'_>, area: Rect, status: &Status<'_>) {
    let height = usize::from(area.height.saturating_sub(2));
    let anchor = if status.focus == Focus::Playlist {
        status.selected_index
    } else {
        status.active_index.unwrap_or(status.selected_index)
    };
    let width = usize::from(area.width.saturating_sub(2));
    let rows = playlist_window(anchor, height, status.playlist.len())
        .map(|index| {
            let entry = &status.playlist[index];
            let marker = if Some(index) == status.active_index {
                "▶"
            } else if index < status.next_index {
                "✓"
            } else {
                "·"
            };
            let cursor = if status.focus == Focus::Playlist && index == status.selected_index {
                ">"
            } else {
                " "
            };
            let locked = if Some(index) == status.active_index {
                " LOCKED"
            } else {
                ""
            };
            let prefix = format!("{cursor}{marker} {} ", index + 1);
            let suffix = format!("  {}{locked}", format_time(entry.duration));
            let title_width = width.saturating_sub(prefix.chars().count() + suffix.chars().count());
            Line::from(format!(
                "{prefix}{}{suffix}",
                truncate_title(&entry.title, title_width)
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rows).block(panel("PLAYLIST")), area);
}

fn render_help(frame: &mut Frame<'_>, area: Rect, focus: Focus) {
    let lines = match focus {
        Focus::Playback => vec![
            Line::from("SPACE PAUSE · ←/→ SEEK · ↑/↓ GAIN · P/N TRACK"),
            Line::from("ENTER START · A ADD · TAB EDIT · Q EXIT"),
        ],
        Focus::Playlist => vec![
            Line::from("↑/↓ SELECT · SHIFT+↑/↓ MOVE · D DELETE"),
            Line::from("P/N TRACK · A ADD · TAB/ESC DONE · Q EXIT"),
        ],
    };
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .block(panel("CONTROLS")),
        area,
    );
}

fn chooser_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(76);
    let height = area.height.saturating_sub(4).min(20);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn render_chooser(frame: &mut Frame<'_>, chooser: &FileChooser) {
    let area = chooser_area(frame.area());
    let entry_height = usize::from(area.height.saturating_sub(6));
    let mut lines = vec![Line::from(format!(
        "PATH {}",
        chooser.directory.to_string_lossy()
    ))];
    let window = playlist_window(chooser.selected, entry_height, chooser.entries.len());
    lines.extend(window.map(|index| {
        let entry = &chooser.entries[index];
        let cursor = if index == chooser.selected { ">" } else { " " };
        let kind = match entry.kind {
            FileKind::Parent => "UP  ",
            FileKind::Directory => "DIR ",
            FileKind::File => "FILE",
        };
        Line::from(format!("{cursor} {kind} {}", entry.label))
    }));
    while lines.len() < entry_height + 1 {
        lines.push(Line::default());
    }
    if let Some(failure) = &chooser.error {
        lines.push(Line::from(Span::styled(
            failure,
            Style::default().add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::default());
    }
    lines.push(Line::from(
        "↑/↓ SELECT · ENTER OPEN/ADD · BACKSPACE PARENT · ESC CANCEL",
    ));

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("ADD VIDEO"))
            .style(Style::default().fg(AMBER).bg(BLACK)),
        area,
    );
}

fn render_main(frame: &mut Frame<'_>, area: Rect, status: &Status<'_>) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Min(6),
        Constraint::Length(4),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("OWNCAST", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" · SHOW LOCAL - AUTO MODE"),
        ])),
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
    render_playlist(frame, rows[5], status);
    render_help(frame, rows[6], status.focus);
}

pub(crate) fn render(frame: &mut Frame<'_>, status: &Status<'_>) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(AMBER).bg(BLACK)),
        area,
    );
    render_main(frame, area, status);
}

pub(crate) fn run(
    terminal: &mut DefaultTerminal,
    session: &mut StreamSession,
    startup_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut focus = Focus::Playback;
    let mut chooser: Option<FileChooser> = None;
    loop {
        session.poll()?;
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
                    playlist: session.entries(),
                    active_index: session.active_index(),
                    selected_index: session.selected_index(),
                    next_index: session.next_index(),
                    focus,
                },
            );
            if let Some(chooser) = &chooser {
                render_chooser(frame, chooser);
            }
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
        if key.code == KeyCode::Char('q')
            || key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return Ok(());
        }
        if chooser.is_some() {
            match chooser_command_for_key(key) {
                Some(ChooserCommand::Select(delta)) => {
                    chooser.as_mut().unwrap().select_by(delta);
                }
                Some(ChooserCommand::Activate) => {
                    let selected = chooser.as_mut().unwrap().activate_selected();
                    match selected {
                        Ok(Some(path)) => match media::discover(&path, None, None) {
                            Ok(entry) => {
                                session.add_entry(entry);
                                chooser = None;
                            }
                            Err(failure) => {
                                chooser.as_mut().unwrap().error = Some(failure.to_string());
                            }
                        },
                        Ok(None) => {}
                        Err(failure) => {
                            chooser.as_mut().unwrap().error = Some(failure.to_string());
                        }
                    }
                }
                Some(ChooserCommand::Parent) => {
                    if let Err(failure) = chooser.as_mut().unwrap().go_to_parent() {
                        chooser.as_mut().unwrap().error = Some(failure.to_string());
                    }
                }
                Some(ChooserCommand::Cancel) => chooser = None,
                None => {}
            }
            continue;
        }
        match command_for_key(focus, session.state(), key) {
            Some(Command::Start) => session.start()?,
            Some(Command::TogglePause) => session.toggle_pause()?,
            Some(Command::Seek(seconds)) => session.seek_by(seconds)?,
            Some(Command::Gain(steps)) => session.adjust_gain(steps),
            Some(Command::PreviousTrack) => session.previous_track()?,
            Some(Command::NextTrack) => session.next_track()?,
            Some(Command::TogglePlaylistFocus) => {
                focus = match focus {
                    Focus::Playback => Focus::Playlist,
                    Focus::Playlist => Focus::Playback,
                };
            }
            Some(Command::Select(delta)) => session.select_entry(delta),
            Some(Command::Move(delta)) => session.move_selected(delta),
            Some(Command::Remove) => session.remove_selected(),
            Some(Command::OpenChooser) => {
                let directory = session
                    .active_path()
                    .and_then(Path::parent)
                    .unwrap_or(startup_dir);
                chooser = Some(match FileChooser::open(directory) {
                    Ok(chooser) => chooser,
                    Err(failure) => FileChooser {
                        directory: directory.to_owned(),
                        entries: Vec::new(),
                        selected: 0,
                        error: Some(failure.to_string()),
                    },
                });
            }
            Some(Command::Quit) => return Ok(()),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaInfo;
    use crate::pipeline::AudioLevels;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        buffer::Buffer,
        crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    };
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn region_text(buffer: &Buffer, area: Rect) -> String {
        (area.y..area.bottom())
            .flat_map(|y| (area.x..area.right()).map(move |x| buffer[(x, y)].symbol()))
            .collect()
    }

    fn row_text(buffer: &Buffer, y: u16, width: u16) -> String {
        region_text(buffer, Rect::new(0, y, width, 1))
    }

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
            playlist: &[],
            active_index: None,
            selected_index: 0,
            next_index: 0,
            focus: Focus::Playback,
        }
    }

    #[test]
    fn maps_only_valid_keys_for_each_playback_state() {
        let command = |state, code| {
            command_for_key(
                Focus::Playback,
                state,
                KeyEvent::new(code, KeyModifiers::NONE),
            )
        };
        for state in [
            PlaybackState::Lobby,
            PlaybackState::Playing,
            PlaybackState::Paused,
        ] {
            assert_eq!(command(state, KeyCode::Up), Some(Command::Gain(1)));
            assert_eq!(command(state, KeyCode::Down), Some(Command::Gain(-1)));
        }
        assert_eq!(
            command(PlaybackState::Lobby, KeyCode::Enter),
            Some(Command::Start)
        );
        assert_eq!(
            command(PlaybackState::Lobby, KeyCode::Char('q')),
            Some(Command::Quit)
        );
        assert_eq!(command(PlaybackState::Lobby, KeyCode::Char(' ')), None);
        assert_eq!(command(PlaybackState::Lobby, KeyCode::Left), None);
        assert_eq!(
            command(PlaybackState::Playing, KeyCode::Char(' ')),
            Some(Command::TogglePause)
        );
        assert_eq!(
            command(PlaybackState::Paused, KeyCode::Char(' ')),
            Some(Command::TogglePause)
        );
        assert_eq!(
            command(PlaybackState::Playing, KeyCode::Left),
            Some(Command::Seek(-30))
        );
        assert_eq!(
            command(PlaybackState::Paused, KeyCode::Right),
            Some(Command::Seek(30))
        );
        assert_eq!(command(PlaybackState::Playing, KeyCode::Char('x')), None);
    }

    #[test]
    fn playback_focus_maps_track_and_playlist_keys() {
        assert_eq!(
            command_for_key(
                Focus::Playback,
                PlaybackState::Playing,
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            ),
            Some(Command::NextTrack)
        );
        assert_eq!(
            command_for_key(
                Focus::Playback,
                PlaybackState::Paused,
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
            ),
            Some(Command::PreviousTrack)
        );
        assert_eq!(
            command_for_key(
                Focus::Playback,
                PlaybackState::Playing,
                KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            ),
            Some(Command::TogglePlaylistFocus)
        );
    }

    #[test]
    fn playlist_focus_maps_selection_reorder_and_remove() {
        assert_eq!(
            command_for_key(
                Focus::Playlist,
                PlaybackState::Playing,
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            ),
            Some(Command::Select(1))
        );
        assert_eq!(
            command_for_key(
                Focus::Playlist,
                PlaybackState::Playing,
                KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT),
            ),
            Some(Command::Move(-1))
        );
        assert_eq!(
            command_for_key(
                Focus::Playlist,
                PlaybackState::Playing,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            ),
            Some(Command::Remove)
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
        let buffer = terminal.backend().buffer();
        let text = region_text(buffer, Rect::new(0, 0, 110, 28));
        let left_meter = row_text(buffer, 8, 110);
        let right_meter = row_text(buffer, 9, 110);

        assert!(text.contains("OWNCAST"));
        assert!(text.contains("SHOW LOCAL - AUTO MODE"));
        assert!(text.contains("SHOW INFORMATION"));
        assert!(text.contains("TITLE: PASSENGER"));
        assert!(text.contains("STATUS: PLAYING"));
        assert!(text.contains("00:42:18 / 01:47:03"));
        assert!(text.contains("GAIN +3 dB"));
        assert!(text.contains("PROGRAM AUDIO / 3 SEC PEAK HOLD"));
        assert!(left_meter.contains("L "));
        assert!(left_meter.contains('█'));
        assert!(left_meter.contains('┃'));
        assert!(left_meter.contains("-4.2 dB"));
        assert!(left_meter.find('┃') > left_meter.rfind('█'));
        assert!(right_meter.contains("R "));
        assert!(right_meter.contains('█'));
        assert!(right_meter.contains('┃'));
        assert!(right_meter.contains("-6.1 dB"));
        assert!(right_meter.find('┃') > right_meter.rfind('█'));
        for required in [
            "PLAYLIST",
            "SPACE PAUSE",
            "←/→ SEEK",
            "↑/↓ GAIN",
            "P/N TRACK",
            "ENTER START",
            "A ADD",
            "TAB EDIT",
            "Q EXIT",
        ] {
            assert!(text.contains(required), "missing console value {required}");
        }
        for y in (0..28).filter(|y| ![4, 5].contains(y)) {
            assert!(
                (0..110).all(|x| buffer[(x, y)].bg == BLACK),
                "row {y} did not retain the black console background"
            );
        }
        assert_eq!(buffer[(5, 4)].fg, BLACK);
        assert_eq!(buffer[(5, 4)].bg, AMBER);
        assert!(buffer.content().iter().any(|cell| cell.fg == AMBER));
    }

    #[test]
    fn narrow_console_keeps_core_status_and_controls() {
        let mut terminal = Terminal::new(TestBackend::new(60, 34)).unwrap();
        terminal.draw(|frame| render(frame, &status())).unwrap();
        let buffer = terminal.backend().buffer();
        let text = region_text(buffer, Rect::new(0, 0, 60, 34));

        for required in [
            "TITLE: PASSENGER",
            "STATUS: PLAYING",
            "00:42:18 / 01:47:03",
            "GAIN +3 dB",
            "PROGRAM AUDIO / 3 SEC PEAK HOLD",
            "-4.2 dB",
            "-6.1 dB",
            "PLAYLIST",
            "SPACE PAUSE",
            "←/→ SEEK",
            "↑/↓ GAIN",
            "P/N TRACK",
            "ENTER START",
            "A ADD",
            "TAB EDIT",
            "Q EXIT",
        ] {
            assert!(text.contains(required), "missing console value {required}");
        }
        for (channel, value) in [("L ", "-4.2 dB"), ("R ", "-6.1 dB")] {
            let (y, meter) = (0..34)
                .map(|y| (y, row_text(buffer, y, 60)))
                .find(|(_, row)| row.contains(value))
                .unwrap();
            assert!(
                meter.contains(channel),
                "meter row {y} does not label {value} as {channel}"
            );
            assert!(meter.contains('█'), "meter row {y} has no current fill");
            assert!(meter.contains('┃'), "meter row {y} has no held marker");
            assert!(
                meter.find('┃') > meter.rfind('█'),
                "meter row {y} held marker is not beyond the current fill"
            );
        }
    }

    #[test]
    fn standard_80x24_console_keeps_status_meter_gain_and_controls_visible() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &status())).unwrap();
        let buffer = terminal.backend().buffer();
        let status_area = region_text(buffer, Rect::new(0, 4, 80, 2));
        let meter_area = region_text(buffer, Rect::new(0, 6, 80, 5));
        let controls_area = region_text(buffer, Rect::new(0, 20, 80, 4));
        let playlist_area = region_text(buffer, Rect::new(0, 11, 80, 9));

        for required in ["STATUS: PLAYING", "GAIN +3 dB", "00:42:18 / 01:47:03"] {
            assert!(
                status_area.contains(required),
                "missing status value {required}"
            );
        }
        for required in ["PROGRAM AUDIO", "L ", "-4.2 dB", "R ", "-6.1 dB"] {
            assert!(
                meter_area.contains(required),
                "missing stereo meter value {required}"
            );
        }
        for required in [
            "SPACE PAUSE",
            "←/→ SEEK",
            "↑/↓ GAIN",
            "P/N TRACK",
            "ENTER START",
            "A ADD",
            "TAB EDIT",
            "Q EXIT",
        ] {
            assert!(
                controls_area.contains(required),
                "missing unclipped control help {required}"
            );
        }
        assert!(playlist_area.contains("PLAYLIST"));
    }

    #[test]
    fn playlist_window_keeps_selected_row_visible() {
        assert_eq!(playlist_window(0, 3, 12), 0..3);
        assert_eq!(playlist_window(10, 3, 12), 8..11);
        assert_eq!(playlist_window(11, 3, 12), 9..12);
        assert_eq!(playlist_window(0, 0, 12), 0..0);
    }

    #[test]
    fn title_truncation_is_character_safe() {
        assert_eq!(truncate_title("Arrival 🛸 Extended", 10), "Arrival 🛸…");
        assert_eq!(truncate_title("Alien", 10), "Alien");
        assert_eq!(truncate_title("Alien", 0), "");
    }

    #[test]
    fn file_chooser_lists_parent_directories_then_files() {
        let root = temp_directory("chooser-order");
        fs::create_dir(root.join("Zulu")).unwrap();
        fs::create_dir(root.join("alpha")).unwrap();
        fs::write(root.join("Beta.mkv"), []).unwrap();
        fs::write(root.join("arrival.mkv"), []).unwrap();

        let chooser = FileChooser::open(&root).unwrap();
        let rows = chooser
            .entries
            .iter()
            .map(|entry| (entry.label.as_str(), entry.kind))
            .collect::<Vec<_>>();

        assert_eq!(
            rows,
            vec![
                ("..", FileKind::Parent),
                ("alpha", FileKind::Directory),
                ("Zulu", FileKind::Directory),
                ("arrival.mkv", FileKind::File),
                ("Beta.mkv", FileKind::File),
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_chooser_enters_directories_and_returns_selected_file() {
        let root = temp_directory("chooser-navigation");
        fs::create_dir(root.join("Movies")).unwrap();
        fs::write(root.join("Movies").join("Alien.mkv"), []).unwrap();
        let mut chooser = FileChooser::open(&root).unwrap();

        chooser.selected = 1;
        assert_eq!(chooser.activate_selected().unwrap(), None);
        assert_eq!(chooser.directory, root.join("Movies"));
        chooser.selected = 1;
        assert_eq!(
            chooser.activate_selected().unwrap(),
            Some(root.join("Movies").join("Alien.mkv"))
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_chooser_keys_take_precedence_over_playback_keys() {
        let command = |code| chooser_command_for_key(KeyEvent::new(code, KeyModifiers::NONE));

        assert_eq!(command(KeyCode::Up), Some(ChooserCommand::Select(-1)));
        assert_eq!(command(KeyCode::Down), Some(ChooserCommand::Select(1)));
        assert_eq!(command(KeyCode::Enter), Some(ChooserCommand::Activate));
        assert_eq!(command(KeyCode::Backspace), Some(ChooserCommand::Parent));
        assert_eq!(command(KeyCode::Esc), Some(ChooserCommand::Cancel));
        assert_eq!(command(KeyCode::Char('q')), None);
        assert_eq!(command(KeyCode::Char('n')), None);
    }

    #[test]
    fn file_chooser_renders_selection_path_help_and_inline_error() {
        let root = temp_directory("chooser-render");
        fs::write(root.join("Alien.mkv"), []).unwrap();
        let mut chooser = FileChooser::open(&root).unwrap();
        chooser.selected = 1;
        chooser.error = Some("Cannot discover video Alien.mkv".to_owned());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|frame| {
                render(frame, &status());
                render_chooser(frame, &chooser);
            })
            .unwrap();
        let text = region_text(terminal.backend().buffer(), Rect::new(0, 0, 80, 24));

        for required in [
            "ADD VIDEO",
            root.to_string_lossy().as_ref(),
            "> FILE Alien.mkv",
            "ENTER OPEN/ADD",
            "BACKSPACE PARENT",
            "ESC CANCEL",
            "Cannot discover video Alien.mkv",
        ] {
            assert!(text.contains(required), "missing chooser value {required}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn temp_directory(label: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("owncast-{label}-{suffix}"));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn standard_console_shows_persistent_playlist_and_compact_help() {
        let playlist = vec![
            MediaInfo {
                path: "/tmp/Passenger.mkv".into(),
                subtitles: None,
                title: "Passenger".into(),
                duration: gst::ClockTime::from_seconds(6_423),
            },
            MediaInfo {
                path: "/tmp/Alien.mkv".into(),
                subtitles: None,
                title: "Alien".into(),
                duration: gst::ClockTime::from_seconds(7_020),
            },
            MediaInfo {
                path: "/tmp/Arrival.mkv".into(),
                subtitles: None,
                title: "Arrival".into(),
                duration: gst::ClockTime::from_seconds(6_960),
            },
        ];
        let status = Status {
            playlist: &playlist,
            active_index: Some(1),
            selected_index: 1,
            next_index: 2,
            focus: Focus::Playback,
            ..status()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal.draw(|frame| render(frame, &status)).unwrap();
        let text = region_text(terminal.backend().buffer(), Rect::new(0, 0, 80, 24));

        for required in [
            "PLAYLIST",
            "✓ 1 Passenger",
            "▶ 2 Alien",
            "LOCKED",
            "· 3 Arrival",
            "P/N TRACK",
            "A ADD",
            "TAB EDIT",
        ] {
            assert!(text.contains(required), "missing {required}");
        }
    }
}
