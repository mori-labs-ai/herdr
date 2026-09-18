use std::{
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use tokio::io::AsyncReadExt;

use super::{
    state::{AppState, TabBarStatusColor, TabBarStatusSegment, TabBarStatusSlot, TabBarStatusSpan},
    App,
};
use crate::config::{TabBarSide, TabBarStatusEntryConfig};

const DATETIME_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_COMMAND_LINE_BYTES: usize = 4096;
const MAX_STATUS_TEXT_CHARS: usize = 80;

pub(super) struct TabBarDatetimeRuntime {
    slot: TabBarStatusSlot,
    format: time::format_description::OwnedFormatItem,
}

pub(super) struct TabBarCommandRuntime {
    slot: TabBarStatusSlot,
    command: String,
    interval: Duration,
    timeout: Duration,
    ansi: bool,
    next_run_at: std::time::Instant,
    task: Option<StatusCommandTask>,
}

impl Drop for TabBarCommandRuntime {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort_handle.abort();
            // Kill the process group on the reconfiguring thread instead of waiting
            // for Tokio to schedule cancellation of the command task.
            task.control.terminate();
        }
    }
}

/// Both tab row status areas resolve through this one function, keyed by side.
fn segments_mut(state: &mut AppState, side: TabBarSide) -> &mut Vec<TabBarStatusSegment> {
    match side {
        TabBarSide::Left => &mut state.tab_bar_left,
        TabBarSide::Right => &mut state.tab_bar_right,
    }
}

impl App {
    pub(super) fn configure_tab_bar_status(
        &mut self,
        left: (&[TabBarStatusEntryConfig], &str),
        right: (&[TabBarStatusEntryConfig], &str),
    ) {
        self.tab_bar_status_generation = self.tab_bar_status_generation.wrapping_add(1);
        self.tab_bar_datetimes.clear();
        self.tab_bar_commands.clear();

        let now = std::time::Instant::now();
        for (side, entries, separator) in [
            (TabBarSide::Left, left.0, left.1),
            (TabBarSide::Right, right.0, right.1),
        ] {
            let segments = self.resolve_tab_bar_status_entries(side, entries, now);
            *segments_mut(&mut self.state, side) = segments;
            match side {
                TabBarSide::Left => {
                    self.state.tab_bar_left_separator = sanitize_separator(separator)
                }
                TabBarSide::Right => {
                    self.state.tab_bar_right_separator = sanitize_separator(separator)
                }
            }
        }

        self.next_tab_bar_datetime_refresh =
            (!self.tab_bar_datetimes.is_empty()).then_some(now + DATETIME_REFRESH_INTERVAL);
    }

    fn resolve_tab_bar_status_entries(
        &mut self,
        side: TabBarSide,
        entries: &[TabBarStatusEntryConfig],
        now: std::time::Instant,
    ) -> Vec<TabBarStatusSegment> {
        let mut segments = Vec::new();
        for entry in entries
            .iter()
            .take(crate::config::MAX_TAB_BAR_STATUS_ENTRIES)
        {
            match entry {
                TabBarStatusEntryConfig::Zoom => {
                    segments.push(TabBarStatusSegment::Zoom);
                }
                TabBarStatusEntryConfig::Hostname => {
                    segments.push(TabBarStatusSegment::Text(sanitize_status_text(
                        crate::platform::hostname().as_deref().unwrap_or_default(),
                    )));
                }
                TabBarStatusEntryConfig::Datetime { format } => {
                    let Ok(format) = crate::config::parse_tab_bar_datetime_format(format) else {
                        continue;
                    };
                    let value = format_local_datetime(&format);
                    let slot = TabBarStatusSlot {
                        side,
                        index: segments.len(),
                    };
                    segments.push(TabBarStatusSegment::Text(value));
                    self.tab_bar_datetimes
                        .push(TabBarDatetimeRuntime { slot, format });
                }
                TabBarStatusEntryConfig::Text { text, fg, bg, bold } => {
                    let fg = entry_color(fg.as_deref());
                    let bg = entry_color(bg.as_deref());
                    if fg.is_none() && bg.is_none() && !bold {
                        segments.push(TabBarStatusSegment::Text(sanitize_literal_text(text)));
                        continue;
                    }
                    let spans = sanitize_literal_text(text)
                        .map(|text| {
                            vec![TabBarStatusSpan {
                                text,
                                fg,
                                bg,
                                bold: *bold,
                            }]
                        })
                        .unwrap_or_default();
                    segments.push(TabBarStatusSegment::Spans(spans));
                }
                TabBarStatusEntryConfig::Command {
                    command,
                    interval_seconds,
                    timeout_seconds,
                    ansi,
                } => {
                    if !crate::platform::status_commands_supported()
                        || command.trim().is_empty()
                        || *interval_seconds == 0
                        || *interval_seconds > crate::config::MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS
                        || *timeout_seconds == 0
                        || *timeout_seconds > crate::config::MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS
                    {
                        continue;
                    }
                    let slot = TabBarStatusSlot {
                        side,
                        index: segments.len(),
                    };
                    segments.push(TabBarStatusSegment::Spans(Vec::new()));
                    self.tab_bar_commands.push(TabBarCommandRuntime {
                        slot,
                        command: command.clone(),
                        interval: Duration::from_secs(*interval_seconds),
                        timeout: Duration::from_secs(*timeout_seconds),
                        ansi: *ansi,
                        next_run_at: now,
                        task: None,
                    });
                }
            }
        }
        segments
    }

    pub(crate) fn handle_tab_bar_status_tasks(&mut self, now: std::time::Instant) -> bool {
        let mut changed = false;

        if self
            .next_tab_bar_datetime_refresh
            .is_some_and(|deadline| now >= deadline)
        {
            for runtime in &self.tab_bar_datetimes {
                let value = format_local_datetime(&runtime.format);
                if let Some(TabBarStatusSegment::Text(current)) =
                    segments_mut(&mut self.state, runtime.slot.side).get_mut(runtime.slot.index)
                {
                    changed |= *current != value;
                    *current = value;
                }
            }
            self.next_tab_bar_datetime_refresh = Some(now + DATETIME_REFRESH_INTERVAL);
        }

        let command_due = self
            .tab_bar_commands
            .iter()
            .any(|runtime| runtime.task.is_none() && now >= runtime.next_run_at);
        if !command_due {
            return changed;
        }

        let generation = self.tab_bar_status_generation;
        let (environment, cwd) = self.custom_command_env();
        for runtime in &mut self.tab_bar_commands {
            if runtime.task.is_some() || now < runtime.next_run_at {
                continue;
            }
            runtime.next_run_at = now.checked_add(runtime.interval).unwrap_or(now);
            runtime.task = Some(spawn_status_command(
                self.event_tx.clone(),
                generation,
                runtime.slot,
                runtime.command.clone(),
                runtime.timeout,
                runtime.ansi,
                environment.clone(),
                cwd.clone(),
            ));
        }

        changed
    }

    pub(crate) fn next_tab_bar_status_deadline(&self) -> Option<std::time::Instant> {
        self.tab_bar_commands
            .iter()
            .filter(|runtime| runtime.task.is_none())
            .map(|runtime| runtime.next_run_at)
            .chain(self.next_tab_bar_datetime_refresh)
            .min()
    }

    pub(super) fn handle_tab_bar_command_finished(
        &mut self,
        generation: u64,
        slot: TabBarStatusSlot,
        result: Result<Option<Vec<TabBarStatusSpan>>, String>,
    ) -> bool {
        if generation != self.tab_bar_status_generation {
            return false;
        }
        let Some(runtime) = self
            .tab_bar_commands
            .iter_mut()
            .find(|runtime| runtime.slot == slot)
        else {
            return false;
        };
        runtime.task = None;

        let output = match result {
            Ok(output) => output.unwrap_or_default(),
            Err(error) => {
                tracing::warn!(command = %runtime.command, error, "tab bar status command failed");
                Vec::new()
            }
        };
        let Some(TabBarStatusSegment::Spans(current)) =
            segments_mut(&mut self.state, slot.side).get_mut(slot.index)
        else {
            return false;
        };
        let changed = *current != output;
        *current = output;
        changed
    }
}

fn entry_color(value: Option<&str>) -> Option<TabBarStatusColor> {
    let (red, green, blue) = crate::config::parse_tab_bar_status_color(value?)?;
    Some(TabBarStatusColor::Rgb(red, green, blue))
}

fn format_local_datetime(format: &time::format_description::OwnedFormatItem) -> Option<String> {
    let datetime = crate::platform::local_datetime()?;
    datetime
        .format(format)
        .ok()
        .and_then(|value| sanitize_status_text(&value))
}

fn sanitize_separator(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn sanitize_literal_text(value: &str) -> Option<String> {
    let value: String = value
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    (!value.is_empty()).then_some(value)
}

fn sanitize_status_text(value: &str) -> Option<String> {
    let value: String = value
        .trim()
        .chars()
        .filter(|character| !character.is_control() && !is_unicode_format_control(*character))
        .take(MAX_STATUS_TEXT_CHARS)
        .collect();
    (!value.is_empty()).then_some(value)
}

/// Apply the plain-text sanitizer across a styled line: drop control and format
/// characters, trim the line's outer whitespace, and cap the total width.
fn sanitize_status_spans(spans: Vec<TabBarStatusSpan>) -> Option<Vec<TabBarStatusSpan>> {
    let mut spans = spans
        .into_iter()
        .map(|mut span| {
            span.text = span
                .text
                .chars()
                .filter(|character| {
                    !character.is_control() && !is_unicode_format_control(*character)
                })
                .collect();
            span
        })
        .collect::<Vec<_>>();

    for span in spans.iter_mut() {
        span.text = span.text.trim_start().to_owned();
        if !span.text.is_empty() {
            break;
        }
    }
    for span in spans.iter_mut().rev() {
        span.text = span.text.trim_end().to_owned();
        if !span.text.is_empty() {
            break;
        }
    }

    let mut remaining = MAX_STATUS_TEXT_CHARS;
    let mut sanitized = Vec::new();
    for mut span in spans {
        if remaining == 0 {
            break;
        }
        if span.text.chars().count() > remaining {
            span.text = span.text.chars().take(remaining).collect();
        }
        remaining -= span.text.chars().count();
        if !span.text.is_empty() {
            sanitized.push(span);
        }
    }
    (!sanitized.is_empty()).then_some(sanitized)
}

fn is_unicode_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

fn command_output_spans(output: &[u8], ansi: bool) -> Option<Vec<TabBarStatusSpan>> {
    if ansi {
        return sanitize_status_spans(ansi_spans(last_output_line(output)));
    }
    let output = String::from_utf8_lossy(output);
    let output = strip_terminal_control_sequences(output.as_bytes());
    let output = String::from_utf8_lossy(&output);
    output
        .lines()
        .next_back()
        .and_then(sanitize_status_text)
        .map(|text| vec![TabBarStatusSpan::plain(text)])
}

fn last_output_line(value: &[u8]) -> &[u8] {
    let value = value.strip_suffix(b"\n").unwrap_or(value);
    match value.iter().rposition(|byte| *byte == b'\n') {
        Some(index) => &value[index + 1..],
        None => value,
    }
}

#[derive(Clone, Copy)]
enum ControlSequenceState {
    Text,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    StString,
}

enum ScanEvent<'a> {
    Text(u8),
    Csi { params: &'a [u8], final_byte: u8 },
}

/// Walk terminal output once, reporting plain bytes and complete CSI sequences.
/// Callers either drop every sequence or interpret the SGR ones.
fn scan_terminal_output(value: &[u8], mut on_event: impl FnMut(ScanEvent)) {
    use ControlSequenceState::*;

    let mut state = Text;
    let mut csi = Vec::new();
    for &byte in value {
        state = match (state, byte) {
            (Text, b'\x1b') => Escape,
            (Text, _) => {
                on_event(ScanEvent::Text(byte));
                Text
            }
            (Escape, b'[') => {
                csi.clear();
                Csi
            }
            (Escape, b']') => Osc,
            (Escape, b'P' | b'X' | b'^' | b'_') => StString,
            (Escape, 0x20..=0x2f) => EscapeIntermediate,
            (Escape, 0x30..=0x7e) => Text,
            (Escape, b'\x1b') => Escape,
            (Escape, b'\x18' | b'\x1a') => Text,
            (Escape, byte) if byte.is_ascii_control() => Escape,
            (Escape, _) => {
                on_event(ScanEvent::Text(byte));
                Text
            }
            (EscapeIntermediate, 0x20..=0x2f) => EscapeIntermediate,
            (EscapeIntermediate, 0x30..=0x7e) => Text,
            (EscapeIntermediate, b'\x1b') => Escape,
            (EscapeIntermediate, b'\x18' | b'\x1a') => Text,
            (EscapeIntermediate, byte) if byte.is_ascii_control() => EscapeIntermediate,
            (EscapeIntermediate, _) => {
                on_event(ScanEvent::Text(byte));
                Text
            }
            (Csi, 0x20..=0x3f) => {
                csi.push(byte);
                Csi
            }
            (Csi, 0x40..=0x7e) => {
                on_event(ScanEvent::Csi {
                    params: &csi,
                    final_byte: byte,
                });
                Text
            }
            (Csi, b'\x1b') => Escape,
            (Csi, b'\x18' | b'\x1a') => Text,
            (Csi, byte) if byte.is_ascii_control() => Csi,
            (Csi, _) => {
                on_event(ScanEvent::Text(byte));
                Text
            }
            (Osc, b'\x07') => Text,
            (Osc, b'\x1b') => Escape,
            (Osc, b'\x18' | b'\x1a') => Text,
            (Osc, _) => Osc,
            (StString, b'\x1b') => Escape,
            (StString, b'\x18' | b'\x1a') => Text,
            (StString, _) => StString,
        };
    }
}

fn strip_terminal_control_sequences(value: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(value.len());
    scan_terminal_output(value, |event| {
        if let ScanEvent::Text(byte) = event {
            output.push(byte);
        }
    });
    output
}

#[derive(Clone, Copy, Default)]
struct SgrStyle {
    fg: Option<TabBarStatusColor>,
    bg: Option<TabBarStatusColor>,
    bold: bool,
}

impl SgrStyle {
    fn span(self, text: Vec<u8>) -> TabBarStatusSpan {
        TabBarStatusSpan {
            text: String::from_utf8_lossy(&text).into_owned(),
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
        }
    }

    fn apply(&mut self, params: &[u32]) {
        let mut index = 0;
        while index < params.len() {
            let param = params[index];
            index += 1;
            match param {
                0 => *self = Self::default(),
                1 => self.bold = true,
                22 => self.bold = false,
                39 => self.fg = None,
                49 => self.bg = None,
                30..=37 => self.fg = Some(TabBarStatusColor::Indexed((param - 30) as u8)),
                90..=97 => self.fg = Some(TabBarStatusColor::Indexed((param - 90 + 8) as u8)),
                40..=47 => self.bg = Some(TabBarStatusColor::Indexed((param - 40) as u8)),
                100..=107 => self.bg = Some(TabBarStatusColor::Indexed((param - 100 + 8) as u8)),
                38 | 48 => {
                    let (color, consumed) = extended_sgr_color(&params[index..]);
                    index += consumed;
                    match (param, color) {
                        (38, Some(color)) => self.fg = Some(color),
                        (_, Some(color)) => self.bg = Some(color),
                        (_, None) => {}
                    }
                }
                _ => {}
            }
        }
    }
}

/// `5;n` selects a palette index and `2;r;g;b` a direct color. A truncated or
/// unknown form consumes the rest of the sequence rather than guessing at it.
fn extended_sgr_color(params: &[u32]) -> (Option<TabBarStatusColor>, usize) {
    match params.first() {
        Some(5) => match params.get(1) {
            Some(&index) if index <= 255 => (Some(TabBarStatusColor::Indexed(index as u8)), 2),
            _ => (None, params.len()),
        },
        Some(2) => match (params.get(1), params.get(2), params.get(3)) {
            (Some(&red), Some(&green), Some(&blue))
                if red <= 255 && green <= 255 && blue <= 255 =>
            {
                (
                    Some(TabBarStatusColor::Rgb(red as u8, green as u8, blue as u8)),
                    4,
                )
            }
            _ => (None, params.len()),
        },
        _ => (None, params.len()),
    }
}

/// Numeric SGR parameters, or `None` when the sequence carries private markers
/// or intermediates and is therefore not a plain `ESC [ ... m`.
fn sgr_params(raw: &[u8]) -> Option<Vec<u32>> {
    if !raw
        .iter()
        .all(|byte| byte.is_ascii_digit() || *byte == b';')
    {
        return None;
    }
    if raw.is_empty() {
        return Some(vec![0]);
    }
    Some(
        raw.split(|byte| *byte == b';')
            .map(|part| {
                if part.is_empty() {
                    return 0;
                }
                std::str::from_utf8(part)
                    .ok()
                    .and_then(|part| part.parse::<u32>().ok())
                    // Out of range parameters are ignored, not reset.
                    .unwrap_or(u32::MAX)
            })
            .collect(),
    )
}

fn ansi_spans(value: &[u8]) -> Vec<TabBarStatusSpan> {
    let mut spans = Vec::new();
    let mut text = Vec::new();
    let mut style = SgrStyle::default();
    scan_terminal_output(value, |event| match event {
        ScanEvent::Text(byte) => text.push(byte),
        ScanEvent::Csi {
            params,
            final_byte: b'm',
        } => {
            let Some(params) = sgr_params(params) else {
                return;
            };
            if !text.is_empty() {
                spans.push(style.span(std::mem::take(&mut text)));
            }
            style.apply(&params);
        }
        ScanEvent::Csi { .. } => {}
    });
    if !text.is_empty() {
        spans.push(style.span(text));
    }
    spans
}

async fn read_last_output_line(
    mut stdout: tokio::process::ChildStdout,
) -> std::io::Result<Vec<u8>> {
    let mut current_line = Vec::new();
    let mut last_line = Vec::new();
    let mut ended_with_newline = false;
    let mut buffer = [0_u8; 1024];

    loop {
        let count = stdout.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        for &byte in &buffer[..count] {
            if byte == b'\n' {
                last_line = std::mem::take(&mut current_line);
                ended_with_newline = true;
            } else {
                if current_line.len() < MAX_COMMAND_LINE_BYTES {
                    current_line.push(byte);
                }
                ended_with_newline = false;
            }
        }
    }

    Ok(if ended_with_newline {
        last_line
    } else {
        current_line
    })
}

struct StatusCommandTask {
    abort_handle: tokio::task::AbortHandle,
    control: Arc<StatusCommandControl>,
}

struct StatusCommandControl {
    terminated: AtomicBool,
    process_group: Mutex<Option<crate::platform::StatusCommandGuard>>,
}

impl StatusCommandControl {
    fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }

    fn terminate(&self) {
        self.terminated.store(true, Ordering::Release);
        if let Some(mut process_group) = self
            .process_group
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            process_group.terminate();
        }
    }

    fn register(&self, mut process_group: crate::platform::StatusCommandGuard) {
        let mut registered = self
            .process_group
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_terminated() {
            process_group.terminate();
        } else {
            *registered = Some(process_group);
        }
    }
}

#[allow(clippy::too_many_arguments)] // One status command's full spawn context.
fn spawn_status_command(
    event_tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    generation: u64,
    slot: TabBarStatusSlot,
    command: String,
    timeout: Duration,
    ansi: bool,
    environment: Vec<(String, String)>,
    cwd: Option<std::path::PathBuf>,
) -> StatusCommandTask {
    let control = Arc::new(StatusCommandControl {
        terminated: AtomicBool::new(false),
        process_group: Mutex::new(None),
    });
    let task_control = Arc::clone(&control);
    let deadline = tokio::time::Instant::now() + timeout;
    let task = tokio::spawn(async move {
        let result = run_status_command(
            task_control.as_ref(),
            command,
            timeout,
            deadline,
            ansi,
            environment,
            cwd,
        )
        .await;
        task_control.terminate();
        let _ = event_tx
            .send(crate::events::AppEvent::TabBarCommandFinished {
                generation,
                slot,
                result,
            })
            .await;
    });
    StatusCommandTask {
        abort_handle: task.abort_handle(),
        control,
    }
}

async fn run_status_command(
    control: &StatusCommandControl,
    command: String,
    timeout: Duration,
    deadline: tokio::time::Instant,
    ansi: bool,
    environment: Vec<(String, String)>,
    cwd: Option<std::path::PathBuf>,
) -> Result<Option<Vec<TabBarStatusSpan>>, String> {
    if control.is_terminated() || tokio::time::Instant::now() >= deadline {
        return Err(format!("timed out after {}s", timeout.as_secs()));
    }

    let mut process = crate::platform::detached_custom_command_process(&command);
    process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .envs(environment);
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    crate::platform::configure_status_command(&mut process);

    let mut process = tokio::process::Command::from(process);
    process.kill_on_drop(true);
    let mut child = process.spawn().map_err(|error| error.to_string())?;
    let process_group =
        crate::platform::StatusCommandGuard::new(&child).map_err(|error| error.to_string())?;
    control.register(process_group);
    if control.is_terminated() {
        return Err("status command was cancelled".into());
    }

    let operation = async {
        let stdout = child.stdout.take();
        let read_output = async {
            let Some(stdout) = stdout else {
                return std::io::Result::Ok(Vec::new());
            };
            read_last_output_line(stdout).await
        };
        let (status, output) = tokio::join!(child.wait(), read_output);
        let status = status.map_err(|error| error.to_string())?;
        let output = output.map_err(|error| error.to_string())?;
        if status.success() {
            Ok(command_output_spans(&output, ansi))
        } else {
            Err(format!("exited with {status}"))
        }
    };
    match tokio::time::timeout_at(deadline, operation).await {
        Ok(result) => result,
        Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, events::AppEvent};

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    type StatusSides<'a> = (
        (&'a [TabBarStatusEntryConfig], &'a str),
        (&'a [TabBarStatusEntryConfig], &'a str),
    );

    /// Configure only the right side, which is what most of these tests care about.
    fn right<'a>(entries: &'a [TabBarStatusEntryConfig], separator: &'a str) -> StatusSides<'a> {
        ((&[], " "), (entries, separator))
    }

    fn right_slot(index: usize) -> TabBarStatusSlot {
        TabBarStatusSlot {
            side: TabBarSide::Right,
            index,
        }
    }

    fn plain(text: &str) -> Vec<TabBarStatusSpan> {
        vec![TabBarStatusSpan::plain(text.into())]
    }

    #[cfg(unix)]
    const MULTILINE_COMMAND: &str = "printf 'old\\nfinal\\n'";
    #[cfg(windows)]
    const MULTILINE_COMMAND: &str = "echo old & echo final";

    #[cfg(unix)]
    const OVER_CAP_COMMAND: &str = "head -c 5000 /dev/zero | tr '\\0' x; printf '\\nREADY\\n'";

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn unique_temp_path(name: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        std::path::PathBuf::from("/var/tmp").join(format!(
            "herdr-tab-status-{name}-{}-{stamp}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn status_command_reports_its_sanitized_last_line() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_status_command(
            event_tx,
            7,
            right_slot(3),
            MULTILINE_COMMAND.into(),
            Duration::from_secs(2),
            false,
            Vec::new(),
            None,
        );

        let event = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .expect("status command timed out")
            .expect("status command event channel closed");
        assert!(matches!(
            event,
            AppEvent::TabBarCommandFinished {
                generation: 7,
                slot,
                result: Ok(Some(ref output)),
            } if slot == right_slot(3) && *output == plain("final")
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test(flavor = "current_thread")]
    async fn status_command_timeout_starts_before_task_is_polled() {
        let ran = unique_temp_path("ran-after-timeout");
        let command = format!("printf ran > {}", ran.display());
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_status_command(
            event_tx,
            7,
            right_slot(3),
            command,
            Duration::from_secs(1),
            false,
            Vec::new(),
            None,
        );

        std::thread::sleep(Duration::from_millis(1100));
        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("status command timed out")
            .expect("status command event channel closed");
        let command_ran = ran.exists();
        let _ = std::fs::remove_file(ran);
        assert!(matches!(
            event,
            AppEvent::TabBarCommandFinished {
                result: Err(ref error),
                ..
            } if error == "timed out after 1s"
        ));
        assert!(!command_ran, "status command ran after its deadline");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn status_command_drains_large_output_and_keeps_the_last_line() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_status_command(
            event_tx,
            7,
            right_slot(3),
            OVER_CAP_COMMAND.into(),
            Duration::from_secs(2),
            false,
            Vec::new(),
            None,
        );

        let event = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .expect("status command timed out")
            .expect("status command event channel closed");
        assert!(matches!(
            event,
            AppEvent::TabBarCommandFinished {
                result: Ok(Some(ref output)),
                ..
            } if *output == plain("READY")
        ));
    }

    #[test]
    fn stale_command_result_does_not_replace_reloaded_status() {
        let mut app = test_app();
        let commands = [TabBarStatusEntryConfig::Command {
            command: MULTILINE_COMMAND.into(),
            interval_seconds: 5,
            timeout_seconds: 2,
            ansi: false,
        }];
        let (left, r) = right(&commands, " ");
        app.configure_tab_bar_status(left, r);
        let stale_generation = app.tab_bar_status_generation;
        let fresh = [TabBarStatusEntryConfig::Text {
            text: "fresh".into(),
            fg: None,
            bg: None,
            bold: false,
        }];
        let (left, r) = right(&fresh, " ");
        app.configure_tab_bar_status(left, r);

        app.handle_tab_bar_command_finished(
            stale_generation,
            right_slot(0),
            Ok(Some(plain("stale"))),
        );

        assert_eq!(
            app.state.tab_bar_right,
            vec![TabBarStatusSegment::Text(Some("fresh".into()))]
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test(flavor = "current_thread")]
    async fn reload_aborts_an_in_flight_command_task_and_its_descendants() {
        let descendant_started = unique_temp_path("descendant-started");
        let survived = unique_temp_path("survived");
        let command = format!(
            "(printf descendant-started > {}; sleep 0.3; printf survived > {}) & wait",
            descendant_started.display(),
            survived.display()
        );
        let mut app = test_app();
        let commands = [TabBarStatusEntryConfig::Command {
            command,
            interval_seconds: 5,
            timeout_seconds: 20,
            ansi: false,
        }];
        let (left, r) = right(&commands, " ");
        app.configure_tab_bar_status(left, r);
        app.handle_tab_bar_status_tasks(std::time::Instant::now());
        for _ in 0..50 {
            if descendant_started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            descendant_started.exists(),
            "status command descendant did not start"
        );

        let reloaded = [TabBarStatusEntryConfig::Text {
            text: "reloaded".into(),
            fg: None,
            bg: None,
            bold: false,
        }];
        let (left, r) = right(&reloaded, " ");
        app.configure_tab_bar_status(left, r);

        // Task cancellation is delivered when Tokio next polls the task. Block
        // this current-thread test runtime long enough for the descendant to
        // run, proving config reload kills its process group synchronously.
        std::thread::sleep(Duration::from_millis(400));
        let descendant_survived = survived.exists();
        let _ = std::fs::remove_file(&descendant_started);
        let _ = std::fs::remove_file(&survived);
        assert!(!descendant_survived, "status command descendant survived");

        assert!(
            tokio::time::timeout(Duration::from_millis(100), app.event_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn in_flight_command_has_no_second_deadline() {
        let mut app = test_app();
        let commands = [TabBarStatusEntryConfig::Command {
            command: MULTILINE_COMMAND.into(),
            interval_seconds: 5,
            timeout_seconds: 2,
            ansi: false,
        }];
        let (left, r) = right(&commands, " ");
        app.configure_tab_bar_status(left, r);

        let now = std::time::Instant::now();
        assert!(app.next_tab_bar_status_deadline().is_some());
        app.handle_tab_bar_status_tasks(now);

        assert!(app.tab_bar_commands[0].task.is_some());
        assert_eq!(app.next_tab_bar_status_deadline(), None);
    }

    #[test]
    fn datetime_refresh_updates_its_segment_once_per_deadline() {
        let mut app = test_app();
        let entries = [TabBarStatusEntryConfig::Datetime {
            format: "%Y-%m-%d %H:%M:%S".into(),
        }];
        let (left, r) = right(&entries, " ");
        app.configure_tab_bar_status(left, r);
        app.state.tab_bar_right[0] = TabBarStatusSegment::Text(None);
        let deadline = app
            .next_tab_bar_datetime_refresh
            .expect("datetime refresh deadline");

        assert!(app.handle_tab_bar_status_tasks(deadline));
        assert!(matches!(
            &app.state.tab_bar_right[0],
            TabBarStatusSegment::Text(Some(value)) if !value.is_empty()
        ));
        assert!(!app.handle_tab_bar_status_tasks(deadline));
    }

    #[test]
    fn both_sides_resolve_through_one_registry() {
        let mut app = test_app();
        let left_entries = [
            TabBarStatusEntryConfig::Text {
                text: "iris".into(),
                fg: Some("#ff8800".into()),
                bg: None,
                bold: true,
            },
            TabBarStatusEntryConfig::Datetime {
                format: "%H:%M".into(),
            },
        ];
        let right_entries = [TabBarStatusEntryConfig::Datetime {
            format: "%H:%M".into(),
        }];
        app.configure_tab_bar_status((&left_entries, " | "), (&right_entries, " · "));

        assert_eq!(
            app.state.tab_bar_left[0],
            TabBarStatusSegment::Spans(vec![TabBarStatusSpan {
                text: "iris".into(),
                fg: Some(TabBarStatusColor::Rgb(0xff, 0x88, 0x00)),
                bg: None,
                bold: true,
            }])
        );
        assert_eq!(app.state.tab_bar_left_separator, " | ");
        assert_eq!(app.state.tab_bar_right_separator, " · ");
        assert_eq!(
            app.tab_bar_datetimes
                .iter()
                .map(|runtime| runtime.slot)
                .collect::<Vec<_>>(),
            vec![
                TabBarStatusSlot {
                    side: TabBarSide::Left,
                    index: 1,
                },
                right_slot(0),
            ]
        );
    }

    #[test]
    fn unstyled_text_entries_stay_plain_and_bad_colors_are_dropped() {
        let mut app = test_app();
        let entries = [
            TabBarStatusEntryConfig::Text {
                text: "plain".into(),
                fg: None,
                bg: None,
                bold: false,
            },
            TabBarStatusEntryConfig::Text {
                text: "bad".into(),
                fg: Some("blue".into()),
                bg: None,
                bold: true,
            },
        ];
        let (left, r) = right(&entries, " ");
        app.configure_tab_bar_status(left, r);

        assert_eq!(
            app.state.tab_bar_right,
            vec![
                TabBarStatusSegment::Text(Some("plain".into())),
                TabBarStatusSegment::Spans(vec![TabBarStatusSpan {
                    text: "bad".into(),
                    fg: None,
                    bg: None,
                    bold: true,
                }]),
            ]
        );
    }

    #[test]
    fn command_output_uses_sanitized_last_line() {
        assert_eq!(
            command_output_spans(b"old\n win\x1b[31mter\r\n", false),
            Some(plain("winter"))
        );
        assert_eq!(command_output_spans(b"\r\n", false), None);
    }

    #[test]
    fn command_output_strips_ansi_style_sequences() {
        assert_eq!(
            command_output_spans(b"\x1b[32mHELLO\x1b[0m", false),
            Some(plain("HELLO"))
        );
    }

    #[test]
    fn command_output_strips_terminal_control_sequence_families() {
        assert_eq!(
            command_output_spans(
                b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
                false
            ),
            Some(plain("link"))
        );
        assert_eq!(
            command_output_spans(b"\x1bPignored\x1b\\visible\x1b7", false),
            Some(plain("visible"))
        );
        assert_eq!(command_output_spans(b"\x1b[31m\x1b[0m", false), None);
        assert_eq!(
            command_output_spans(b"\x1b\x07[32mHELLO\x1b[0m", false),
            Some(plain("HELLO"))
        );
        assert_eq!(
            command_output_spans(b"\x1bPignored\x18VISIBLE", false),
            Some(plain("VISIBLE"))
        );
        assert_eq!(
            command_output_spans(b"\x1bPignored\x1b7VISIBLE", false),
            Some(plain("VISIBLE"))
        );
        assert_eq!(
            command_output_spans(b"\x1b]ignored\x1aVISIBLE", false),
            Some(plain("VISIBLE"))
        );
        assert_eq!(
            command_output_spans(b"\xc2\x1b[31m\xa2", false),
            Some(plain("��"))
        );

        let styled = format!("\x1b[38;2;1;2;3m{}\x1b[0m", "x".repeat(80));
        assert_eq!(
            command_output_spans(styled.as_bytes(), false),
            Some(plain(&"x".repeat(80)))
        );
    }

    fn spans(value: &str) -> Vec<TabBarStatusSpan> {
        ansi_spans(value.as_bytes())
    }

    fn span(
        text: &str,
        fg: Option<TabBarStatusColor>,
        bg: Option<TabBarStatusColor>,
        bold: bool,
    ) -> TabBarStatusSpan {
        TabBarStatusSpan {
            text: text.into(),
            fg,
            bg,
            bold,
        }
    }

    #[test]
    fn sgr_parses_basic_and_bright_colors() {
        use TabBarStatusColor::Indexed;

        assert_eq!(
            spans("\x1b[31mred\x1b[92mbright\x1b[0mplain"),
            vec![
                span("red", Some(Indexed(1)), None, false),
                span("bright", Some(Indexed(10)), None, false),
                span("plain", None, None, false),
            ]
        );
        assert_eq!(
            spans("\x1b[44mbg\x1b[104mbright\x1b[49mnone"),
            vec![
                span("bg", None, Some(Indexed(4)), false),
                span("bright", None, Some(Indexed(12)), false),
                span("none", None, None, false),
            ]
        );
        assert_eq!(
            spans("\x1b[37;40mboth"),
            vec![span("both", Some(Indexed(7)), Some(Indexed(0)), false)]
        );
    }

    #[test]
    fn sgr_parses_bold_and_its_resets() {
        assert_eq!(
            spans("\x1b[1mbold\x1b[22mnormal"),
            vec![
                span("bold", None, None, true),
                span("normal", None, None, false),
            ]
        );
        assert_eq!(
            spans("\x1b[1;31mboth\x1b[39mkeeps bold"),
            vec![
                span("both", Some(TabBarStatusColor::Indexed(1)), None, true),
                span("keeps bold", None, None, true),
            ]
        );
        // A bare `ESC [ m` is `ESC [ 0 m`.
        assert_eq!(
            spans("\x1b[1mbold\x1b[mreset"),
            vec![
                span("bold", None, None, true),
                span("reset", None, None, false),
            ]
        );
    }

    #[test]
    fn sgr_parses_indexed_and_truecolor_forms() {
        assert_eq!(
            spans("\x1b[38;5;208mfg\x1b[48;5;236mbg"),
            vec![
                span("fg", Some(TabBarStatusColor::Indexed(208)), None, false),
                span(
                    "bg",
                    Some(TabBarStatusColor::Indexed(208)),
                    Some(TabBarStatusColor::Indexed(236)),
                    false
                ),
            ]
        );
        assert_eq!(
            spans("\x1b[38;2;10;20;30;48;2;1;2;3mtrue"),
            vec![span(
                "true",
                Some(TabBarStatusColor::Rgb(10, 20, 30)),
                Some(TabBarStatusColor::Rgb(1, 2, 3)),
                false
            )]
        );
    }

    #[test]
    fn sgr_ignores_unknown_parameters_and_garbage() {
        // Unknown parameters are skipped without disturbing their neighbours.
        assert_eq!(
            spans("\x1b[3;31;53mstyled"),
            vec![span(
                "styled",
                Some(TabBarStatusColor::Indexed(1)),
                None,
                false
            )]
        );
        // Out of range values are ignored rather than treated as a reset.
        assert_eq!(
            spans("\x1b[1m\x1b[99999mstill bold"),
            vec![span("still bold", None, None, true)]
        );
        // A truncated extended color leaves the style alone.
        assert_eq!(
            spans("\x1b[38;5mtruncated"),
            vec![span("truncated", None, None, false)]
        );
        assert_eq!(
            spans("\x1b[38;2;1mtruncated"),
            vec![span("truncated", None, None, false)]
        );
        assert_eq!(
            spans("\x1b[38;5;999mout of range"),
            vec![span("out of range", None, None, false)]
        );
        // Private markers are not plain SGR.
        assert_eq!(
            spans("\x1b[>1mprivate"),
            vec![span("private", None, None, false)]
        );
        // Every other control sequence is still stripped.
        assert_eq!(
            spans("\x1b[2Kcleared\x1b]0;title\x07\x1b[31mred"),
            vec![
                span("cleared", None, None, false),
                span("red", Some(TabBarStatusColor::Indexed(1)), None, false),
            ]
        );
        assert!(spans("\x1b[31m\x1b[0m").is_empty());
    }

    #[test]
    fn ansi_command_output_keeps_styled_spans_of_the_last_line() {
        assert_eq!(
            command_output_spans(b"first\n \x1b[1;31mALERT\x1b[0m ok \n", true),
            Some(vec![
                span("ALERT", Some(TabBarStatusColor::Indexed(1)), None, true),
                span(" ok", None, None, false),
            ])
        );
        assert_eq!(command_output_spans(b"\x1b[31m\x1b[0m\n", true), None);

        // The 80 character cap applies across the whole styled line.
        let long = format!("\x1b[31m{}\x1b[32m{}", "a".repeat(70), "b".repeat(70));
        assert_eq!(
            command_output_spans(long.as_bytes(), true),
            Some(vec![
                span(
                    &"a".repeat(70),
                    Some(TabBarStatusColor::Indexed(1)),
                    None,
                    false
                ),
                span(
                    &"b".repeat(10),
                    Some(TabBarStatusColor::Indexed(2)),
                    None,
                    false
                ),
            ])
        );
    }

    #[test]
    fn styled_status_text_drops_bidi_and_zero_width_format_controls() {
        assert_eq!(
            command_output_spans("safe\u{202e}\x1b[31mevil\u{200b}".as_bytes(), true),
            Some(vec![
                span("safe", None, None, false),
                span("evil", Some(TabBarStatusColor::Indexed(1)), None, false),
            ])
        );
    }

    #[test]
    fn status_text_strips_bidi_and_zero_width_format_controls() {
        assert_eq!(
            sanitize_status_text("safe\u{202e}evil\u{200b}"),
            Some("safeevil".into())
        );
    }

    #[test]
    fn separator_preserves_printable_spacing_and_drops_controls() {
        assert_eq!(sanitize_separator(" \x1b|\n "), " | ");
    }
}
