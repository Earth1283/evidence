//! The live panel and the "saved" summary, drawn with ratatui.

use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};
use ratatui::Frame;

pub const FLOOR_DB: f32 = -60.0;
pub const PANEL_WIDTH: u16 = 100;
const SILENCE_WARNING: &str =
    "⚠ input is dead silent — check System Settings ▸ Privacy & Security ▸ Microphone for your terminal";

const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const GRID: Color = Color::Indexed(239);

pub fn to_db(x: f32) -> f32 {
    if x > 1e-6 {
        20.0 * x.log10()
    } else {
        -120.0
    }
}

pub fn fmt_time(s: f64) -> String {
    let s = s as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// Coarse duration for estimates: '42 min', '3h 20m', '>99h'.
pub fn fmt_span(s: f64) -> String {
    let (h, m) = ((s / 3600.0) as u64, (s % 3600.0 / 60.0) as u64);
    if h > 99 {
        ">99h".into()
    } else if h > 0 {
        format!("~{h}h {m:02}m")
    } else {
        format!("~{} min", m.max(1))
    }
}

pub fn fmt_size(n: u64) -> String {
    let mut n = n as f64;
    for unit in ["B", "KB", "MB"] {
        if n < 1024.0 {
            return if unit == "B" {
                format!("{n:.0} {unit}")
            } else {
                format!("{n:.1} {unit}")
            };
        }
        n /= 1024.0;
    }
    format!("{n:.1} GB")
}

pub fn fmt_khz(rate: u32) -> String {
    format!("{}", rate as f64 / 1000.0)
}

fn layout_name(channels: u16) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        n => format!("{n} ch"),
    }
}

fn channel_labels(n: usize) -> Vec<String> {
    match n {
        1 => vec!["IN".into()],
        2 => vec!["L".into(), "R".into()],
        _ => (1..=n).map(|i| i.to_string()).collect(),
    }
}

fn meter_bar(db: f32, hold_db: f32, width: usize) -> Vec<Span<'static>> {
    let pos = |d: f32| {
        ((d - FLOOR_DB) / -FLOOR_DB * width as f32)
            .round()
            .clamp(0.0, width as f32) as usize
    };
    let (filled, hold) = (pos(db), pos(hold_db));
    (0..width)
        .map(|i| {
            let d = FLOOR_DB + (i as f32 + 0.5) / width as f32 * -FLOOR_DB;
            let color = if d < -18.0 {
                Color::Green
            } else if d < -6.0 {
                Color::Yellow
            } else {
                Color::Red
            };
            if i < filled {
                Span::styled("█", Style::new().fg(color))
            } else if hold > 0 && i == hold - 1 {
                Span::styled("▏", Style::new().fg(color).add_modifier(Modifier::BOLD))
            } else {
                Span::styled("·", Style::new().fg(GRID))
            }
        })
        .collect()
}

/// Everything the live panel shows, snapshotted so drawing never touches the recorder.
#[derive(Clone, Default)]
pub struct LiveState {
    pub device_name: String,
    pub rate: u32,
    pub channels: u16,
    pub elapsed: f64,
    pub size: u64,
    pub paused: bool,
    pub clips: u64,
    pub dropouts: u64,
    pub heard_signal: bool,
    pub note: Option<String>,
    pub error: Option<String>,
}

/// Smoothed level + peak-hold per channel.
pub struct Meters {
    pub level: Vec<f32>,
    pub hold: Vec<f32>,
    hold_at: Vec<Instant>,
    last: Instant,
}

impl Meters {
    pub fn new(channels: usize) -> Self {
        let now = Instant::now();
        Self {
            level: vec![FLOOR_DB; channels],
            hold: vec![FLOOR_DB; channels],
            hold_at: vec![now; channels],
            last: now,
        }
    }

    pub fn update(&mut self, peaks: &[f32]) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32();
        self.last = now;
        for (i, &p) in peaks.iter().enumerate() {
            let db = to_db(p);
            self.level[i] = db.max(self.level[i] - 45.0 * dt);
            if db >= self.hold[i] {
                self.hold[i] = db;
                self.hold_at[i] = now;
            } else if now.duration_since(self.hold_at[i]).as_secs_f32() > 1.5 {
                self.hold[i] = self.level[i].max(self.hold[i] - 30.0 * dt);
            }
        }
    }
}

fn panel(
    title: &'static str,
    border: Color,
    lines: Vec<Line<'static>>,
) -> (Paragraph<'static>, u16) {
    let height = lines.len() as u16 + 4; // border + vertical padding
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(Line::styled(format!(" {title} "), BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    (Paragraph::new(lines).block(block), height)
}

fn spread(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let used: usize = left
        .iter()
        .chain(right.iter())
        .map(|s| s.content.chars().count())
        .sum();
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    spans.extend(right);
    Line::from(spans)
}

/// Number of rows `live_lines` can produce at most, so the inline viewport can be sized once.
pub fn live_height(channels: u16) -> u16 {
    // head, gap, 2 info, gap, rows, gap, stats, (gap, note: 2 lines), (gap, warning: 2 lines), gap, keys
    let inner = 1 + 1 + 2 + 1 + channels + 1 + 1 + 3 + 3 + 1 + 1;
    inner + 4
}

fn wrapped(text: &str, width: usize) -> usize {
    text.chars().count().div_ceil(width.max(1)).max(1)
}

pub fn draw_live(f: &mut Frame, rec: &LiveState, meters: &Meters, blink_on: bool, free: u64) {
    let width = f.area().width.min(PANEL_WIDTH);
    let inner = width.saturating_sub(6) as usize; // borders + horizontal padding
    let error = rec.error.clone();
    let paused = rec.paused;

    let status = if error.is_some() {
        Span::styled(
            "■ ERROR",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if paused {
        Span::styled(
            "❚❚ PAUSED",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            if blink_on { "● REC" } else { "○ REC" },
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };
    let clock = Span::styled(fmt_time(rec.elapsed), BOLD.fg(Color::White));

    let fmt = format!(
        "FLAC · 24-bit · {} kHz · {}",
        fmt_khz(rec.rate),
        layout_name(rec.channels)
    );

    let bar_width = (f.area().width.min(PANEL_WIDTH) as usize)
        .saturating_sub(34)
        .max(10);
    let mut lines = vec![
        spread(vec![status], vec![clock], inner),
        Line::raw(""),
        Line::styled(rec.device_name.clone(), BOLD),
        Line::styled(fmt, Style::new().fg(Color::Cyan)),
        Line::raw(""),
    ];
    for (i, label) in channel_labels(rec.channels as usize)
        .into_iter()
        .enumerate()
    {
        let hold = meters.hold[i];
        let hold_txt = if hold <= FLOOR_DB {
            "-∞".to_string()
        } else {
            format!("{hold:+.1}")
        };
        let style = if hold > -1.0 {
            BOLD.fg(Color::Red)
        } else {
            DIM
        };
        let mut spans = vec![Span::styled(label, BOLD), Span::raw(" ")];
        spans.extend(meter_bar(meters.level[i], hold, bar_width));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!("{hold_txt:>5} dB"), style));
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));

    let size = rec.size;
    let mut stats = vec![
        Span::raw(fmt_size(size)),
        Span::styled(format!("  ·  {} free", fmt_size(free)), DIM),
    ];
    if rec.elapsed > 10.0 && size > 0 {
        let rate = size as f64 / rec.elapsed;
        stats.push(Span::styled(
            format!(" ({} left)", fmt_span(free as f64 / rate)),
            DIM,
        ));
    }
    let (clips, lost) = (rec.clips, rec.dropouts);
    let alert = BOLD.fg(Color::Red);
    stats.push(Span::styled("  ·  clips ", DIM));
    stats.push(Span::styled(
        clips.to_string(),
        if clips > 0 { alert } else { DIM },
    ));
    stats.push(Span::styled("  ·  dropouts ", DIM));
    stats.push(Span::styled(
        lost.to_string(),
        if lost > 0 { alert } else { DIM },
    ));
    lines.push(Line::from(stats));

    let extra = |text: String, color: Color, lines: &mut Vec<Line<'static>>| {
        lines.push(Line::raw(""));
        lines.push(Line::styled(text, Style::new().fg(color)));
    };
    if let Some(note) = &rec.note {
        extra(format!("⚠ {note}"), Color::Yellow, &mut lines);
    }
    if let Some(e) = &error {
        extra(format!("⚠ write failed: {e}"), Color::Red, &mut lines);
    } else if rec.elapsed > 2.0 && !rec.heard_signal {
        extra(SILENCE_WARNING.into(), Color::Yellow, &mut lines);
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("space pause/resume   q stop & save", DIM));

    let border = if !paused && error.is_none() {
        Color::Red
    } else {
        Color::Yellow
    };
    let (para, _) = panel("rec", border, lines.clone());
    let rows: usize = lines
        .iter()
        .map(|l| {
            wrapped(
                &l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>(),
                inner,
            )
        })
        .sum();
    let height = (rows as u16 + 4).min(f.area().height);
    f.render_widget(
        para.wrap(ratatui::widgets::Wrap { trim: false }),
        Rect::new(0, 0, width, height),
    );
}

pub struct Summary {
    pub path: String,
    pub elapsed: f64,
    pub frames: u64,
    pub size: u64,
    pub rate: u32,
    pub channels: u16,
    pub clips: u64,
    pub dropouts: u64,
}

fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn summary_lines(s: &Summary, width: u16) -> Vec<Line<'static>> {
    let label = |l: &str| Span::styled(format!("{l:<8}  "), DIM);
    let fold = (width as usize).saturating_sub(6 + 10).max(1);
    let path: Vec<char> = s.path.chars().collect();
    let mut lines = Vec::new();
    for (i, chunk) in path.chunks(fold).enumerate() {
        let text: String = chunk.iter().collect();
        let head = if i == 0 {
            label("file")
        } else {
            Span::raw(" ".repeat(10))
        };
        lines.push(Line::from(vec![
            head,
            Span::styled(text, BOLD.fg(Color::Green)),
        ]));
    }
    lines.push(Line::from(vec![
        label("length"),
        Span::raw(format!(
            "{}  ({} frames)",
            fmt_time(s.elapsed),
            group_thousands(s.frames)
        )),
    ]));
    lines.push(Line::from(vec![label("size"), Span::raw(fmt_size(s.size))]));
    lines.push(Line::from(vec![
        label("format"),
        Span::raw(format!(
            "FLAC · 24-bit · {} kHz · {} ch",
            fmt_khz(s.rate),
            s.channels
        )),
    ]));
    let red = Style::new().fg(Color::Red);
    if s.clips > 0 {
        lines.push(Line::from(vec![
            label("clips"),
            Span::styled(format!("{} blocks hit full scale", s.clips), red),
        ]));
    }
    if s.dropouts > 0 {
        lines.push(Line::from(vec![
            label("dropouts"),
            Span::styled(format!("{} (audio may have gaps)", s.dropouts), red),
        ]));
    }
    lines
}

pub fn summary_height(s: &Summary, width: u16) -> u16 {
    summary_lines(s, width.min(PANEL_WIDTH)).len() as u16 + 4
}

pub fn draw_summary(f: &mut Frame, s: &Summary) {
    let width = f.area().width.min(PANEL_WIDTH);
    let lines = summary_lines(s, width);
    let (para, height) = panel("saved", Color::Green, lines);
    f.render_widget(para, Rect::new(0, 0, width, height));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(fmt_time(3725.9), "01:02:05");
        assert_eq!(fmt_span(1500.0), "~25 min");
        assert_eq!(fmt_span(12000.0), "~3h 20m");
        assert_eq!(fmt_span(1e9), ">99h");
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(1536), "1.5 KB");
        assert_eq!(fmt_khz(96000), "96");
        assert_eq!(fmt_khz(44100), "44.1");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }

    #[test]
    fn meter_hold_decays_after_delay() {
        let mut m = Meters::new(1);
        m.update(&[0.5]);
        assert!(m.hold[0] > -7.0 && m.level[0] > -7.0);
    }

    #[test]
    fn bar_has_requested_width() {
        assert_eq!(meter_bar(-30.0, -10.0, 40).len(), 40);
    }
}
