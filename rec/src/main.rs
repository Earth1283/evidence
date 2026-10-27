//! rec — high-fidelity microphone recorder (24-bit FLAC) with a live terminal meter.

mod audio;
mod flac;
mod ui;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::{Terminal, TerminalOptions, Viewport};

use audio::{input_devices, pick_device, Recorder};
use ui::{Meters, Summary};

const DEFAULT_RATE: u32 = 96000;
const FPS: u64 = 20;

#[derive(Parser)]
#[command(
    name = "rec",
    about = "High-fidelity microphone recorder (24-bit FLAC)."
)]
struct Args {
    /// File name (default: timestamp); saved in ~/Recordings
    name: Option<String>,
    /// Input device: index or name substring (default: system input)
    #[arg(short, long)]
    device: Option<String>,
    /// Sample rate in Hz
    #[arg(short, long, default_value_t = DEFAULT_RATE)]
    rate: u32,
    /// Channels (default: all, up to 2)
    #[arg(short, long)]
    channels: Option<u16>,
    /// Output directory
    #[arg(short, long)]
    outdir: Option<PathBuf>,
    /// List input devices and exit
    #[arg(short, long)]
    list: bool,
    /// Convert in software if the mic can't do the rate natively
    #[arg(long)]
    resample: bool,
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

fn expand_home(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => p.to_path_buf(),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn list_devices() {
    let devs = input_devices();
    let name_w = devs
        .iter()
        .map(|d| d.name.chars().count() + if d.is_default { 10 } else { 0 })
        .max()
        .unwrap_or(0)
        .max(12);
    println!(
        "\x1b[1m{:>2}  {:<name_w$}  {:>2}  {:>11}\x1b[0m",
        "#", "Input device", "Ch", "Native rate"
    );
    for d in devs {
        let mark = if d.is_default {
            " \x1b[32m●\x1b[0m default"
        } else {
            ""
        };
        let pad = name_w - d.name.chars().count() - if d.is_default { 10 } else { 0 };
        println!(
            "{:>2}  {}{}{}  {:>2}  {:>7} kHz",
            d.index,
            d.name,
            mark,
            " ".repeat(pad),
            d.max_channels,
            ui::fmt_khz(d.native_rate)
        );
    }
}

fn timestamp() -> String {
    // SAFETY: localtime_r only writes into the zeroed `tm` we own.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

fn output_path(name: Option<&str>, outdir: &Path) -> io::Result<PathBuf> {
    let mut p = match name {
        Some(n) => {
            let p = expand_home(Path::new(n));
            if p.parent().is_none_or(|d| d.as_os_str().is_empty()) {
                outdir.join(p)
            } else {
                p
            }
        }
        None => outdir.join(format!("rec-{}", timestamp())),
    };
    p.set_extension("flac");
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let base = p.clone();
    let stem = base
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut n = 2;
    while p.exists() {
        p = base.with_file_name(format!("{stem}-{n}.flac"));
        n += 1;
    }
    Ok(p)
}

/// Puts the terminal in raw mode and restores it on drop (including on panic).
struct RawMode;

impl RawMode {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn inline_terminal(height: u16) -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
}

/// Runs the live panel until the user stops. Returns the raw-mode guard so the caller can
/// keep the terminal in raw mode while drawing the summary (ratatui queries the cursor).
fn run_ui(rec: &mut Recorder) -> io::Result<RawMode> {
    let raw = RawMode::enter()?;
    let mut terminal = inline_terminal(ui::live_height(rec.channels))?;
    let mut meters = Meters::new(rec.channels as usize);
    let started = Instant::now();
    let frame = Duration::from_millis(1000 / FPS);
    let mut result = Ok(());

    let parent = rec.path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut free = 0;
    let mut free_at: Option<Instant> = None;

    'main: while rec.shared.error().is_none() {
        // Disk space is a syscall; once a second is plenty.
        if free_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            free = fs4::available_space(&parent).unwrap_or(0);
            free_at = Some(Instant::now());
        }
        meters.update(&rec.shared.take_peaks());
        let blink_on = (started.elapsed().as_millis() / 500).is_multiple_of(2);
        if let Err(e) =
            terminal.draw(|f| ui::draw_live(f, &rec.live_state(), &meters, blink_on, free))
        {
            result = Err(e);
            break;
        }
        // Waiting on input instead of sleeping keeps keys responsive between frames.
        // Raw mode turns Ctrl+C into a key event, so treat it like q.
        let mut deadline = frame;
        while event::poll(deadline)? {
            deadline = Duration::ZERO;
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q' | 'Q') | KeyCode::Esc => break 'main,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break 'main
                    }
                    KeyCode::Char(' ') => rec.toggle_pause(),
                    _ => {}
                }
            }
        }
    }
    // Transient: wipe the live panel so the summary takes its place.
    // `clear` restores the cursor to the bottom of the viewport; park it at the top so the
    // summary lands where the panel was instead of below a blank gap.
    let origin = terminal.get_frame().area().as_position();
    terminal.clear()?;
    terminal.set_cursor_position(origin)?;
    result.map(|()| raw)
}

fn main() {
    let args = Args::parse();

    if args.list {
        list_devices();
        return;
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        fail("rec needs an interactive terminal.");
    }

    let dev = pick_device(args.device.as_deref()).unwrap_or_else(|e| fail(e));
    let channels = args.channels.unwrap_or_else(|| dev.max_channels.min(2));
    if channels == 0 || channels > dev.max_channels {
        fail(format!(
            "{} only has {} input channel(s).",
            dev.name, dev.max_channels
        ));
    }

    let outdir = args
        .outdir
        .as_deref()
        .map(expand_home)
        .unwrap_or_else(|| home().join("Recordings"));
    let path = output_path(args.name.as_deref(), &outdir)
        .unwrap_or_else(|e| fail(format!("Could not start recording: {e}")));
    let mut rec = Recorder::new(&dev, channels, args.rate, &path, args.resample)
        .unwrap_or_else(|e| fail(format!("Could not start recording: {e}")));

    let ui_result = match rec.start() {
        Ok(()) => run_ui(&mut rec),
        Err(e) => {
            rec.stop();
            fail(format!("Could not start recording: {e}"));
        }
    };
    rec.stop();

    let raw = match ui_result {
        Ok(raw) => Some(raw),
        Err(e) => {
            eprintln!("terminal error: {e}");
            None
        }
    };
    if let Some(e) = rec.shared.error() {
        eprintln!("\x1b[31mWrite failed:\x1b[0m {e}");
    }
    use std::sync::atomic::Ordering::Relaxed;
    let summary = Summary {
        path: rec.path.display().to_string(),
        elapsed: rec.elapsed(),
        frames: rec.frames(),
        size: rec.size(),
        rate: rec.rate,
        channels: rec.channels,
        clips: rec.shared.clips.load(Relaxed),
        dropouts: rec.shared.dropped.load(Relaxed) + rec.shared.overruns.load(Relaxed),
    };
    let (width, _) = ratatui::crossterm::terminal::size().unwrap_or((100, 24));
    if raw.is_some() {
        if let Ok(mut terminal) = inline_terminal(ui::summary_height(&summary, width)) {
            let _ = terminal.draw(|f| ui::draw_summary(f, &summary));
        }
    }
    drop(raw);
    println!();
}
