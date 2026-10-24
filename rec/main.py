#!/usr/bin/env python3
import argparse
import math
import os
import queue
import select
import shutil
import sys
import termios
import threading
import time
import tty
from datetime import datetime
from pathlib import Path

import numpy as np
import sounddevice as sd
import soundfile as sf
from rich.console import Console, Group
from rich.live import Live
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

DEFAULT_RATE = 96000
DEFAULT_DIR = Path.home() / "Recordings"
BLOCK = 4096            # frames per callback (~43 ms at 96 kHz)
QUEUE_BLOCKS = 4096     # ~30 min of slack if the disk stalls
FLOOR_DB = -60.0
CLIP_LEVEL = 0.999
FPS = 20

console = Console()


def input_devices():
    return [d for d in sd.query_devices() if d["max_input_channels"] > 0]


def list_devices():
    default = sd.default.device[0]
    table = Table(box=None, header_style="bold", pad_edge=False)
    table.add_column("#", justify="right")
    table.add_column("Input device")
    table.add_column("Ch", justify="right")
    table.add_column("Native rate", justify="right")
    for d in input_devices():
        mark = " [green]●[/] default" if d["index"] == default else ""
        table.add_row(
            str(d["index"]),
            d["name"] + mark,
            str(d["max_input_channels"]),
            f"{d['default_samplerate'] / 1000:g} kHz",
        )
    console.print(table)


def pick_device(spec):
    """Resolve -d (index or name substring) to a device dict."""
    devs = input_devices()
    if not devs:
        sys.exit("No input devices found.")
    if spec is None:
        idx = sd.default.device[0]
        if idx is None or idx < 0:
            return devs[0]
        return sd.query_devices(idx)
    if spec.isdigit():
        for d in devs:
            if d["index"] == int(spec):
                return d
        sys.exit(f"No input device with index {spec}. Try: rec -l")
    matches = [d for d in devs if spec.lower() in d["name"].lower()]
    if len(matches) == 1:
        return matches[0]
    if not matches:
        sys.exit(f"No input device matching '{spec}'. Try: rec -l")
    names = ", ".join(f"{d['index']}:{d['name']}" for d in matches)
    sys.exit(f"'{spec}' is ambiguous: {names}")


def core_audio(strict):
    """Switch the device to the requested rate; optionally refuse resampling."""
    if sys.platform != "darwin":
        return None
    return sd.CoreAudioSettings(change_device_parameters=True,
                                fail_if_conversion_required=strict)


def open_stream(dev, channels, rate, resample, callback):
    """Open at the requested rate if the hardware supports it natively.

    Returns (stream, actual_rate, note). Falls back to the device's native rate
    rather than silently resampling, unless resample=True.
    """
    def attempt(r, strict):
        return sd.InputStream(
            device=dev["index"], channels=channels, samplerate=r, dtype="float32",
            blocksize=BLOCK, latency="high", callback=callback,
            extra_settings=core_audio(strict),
        )

    try:
        return attempt(rate, True), rate, None
    except sd.PortAudioError:
        pass
    if resample:
        return attempt(rate, False), rate, f"resampled to {rate / 1000:g} kHz by CoreAudio"
    native = int(dev["default_samplerate"])
    try:
        stream = attempt(native, True)
    except sd.PortAudioError:
        stream = attempt(native, False)
    note = (f"device can't do {rate / 1000:g} kHz natively; "
            f"recording at native {native / 1000:g} kHz (use --resample to force)")
    return stream, native, note


class Recorder:
    def __init__(self, dev, channels, rate, path, resample):
        self.dev, self.channels, self.path = dev, channels, path
        self.paused = False
        self.dropped = 0          # blocks lost because the writer queue was full
        self.overruns = 0         # PortAudio input overflows
        self.clips = 0
        self.heard_signal = False
        self.frames = 0           # frames written to disk
        self.error = None
        self.block_peak = np.zeros(channels)
        self.q = queue.Queue(maxsize=QUEUE_BLOCKS)

        self.stream, self.rate, self.note = open_stream(
            dev, channels, rate, resample, self._callback)
        self.file = sf.SoundFile(path, "w", samplerate=self.rate, channels=channels,
                                 format="FLAC", subtype="PCM_24")
        self.writer = threading.Thread(target=self._write_loop, daemon=True)

    def _callback(self, indata, frames, time_info, status):
        if status.input_overflow:
            self.overruns += 1
        peaks = np.abs(indata).max(axis=0)
        self.block_peak = np.maximum(self.block_peak, peaks)
        if peaks.max() > 0:
            self.heard_signal = True
        if self.paused:
            return
        if peaks.max() >= CLIP_LEVEL:
            self.clips += 1
        try:
            self.q.put_nowait(indata.copy())
        except queue.Full:
            self.dropped += 1

    def _write_loop(self):
        try:
            while True:
                block = self.q.get()
                if block is None:
                    break
                self.file.write(block)
                self.frames += len(block)
        except Exception as e:  # disk full, unplugged volume, ...
            self.error = e
        finally:
            self.file.close()

    def start(self):
        self.writer.start()
        self.stream.start()

    def stop(self):
        self.stream.stop()
        self.stream.close()
        self.q.put(None)
        self.writer.join()

    def take_peaks(self):
        peaks, self.block_peak = self.block_peak, np.zeros(self.channels)
        return peaks

    @property
    def elapsed(self):
        return self.frames / self.rate

    @property
    def size(self):
        try:
            return os.path.getsize(self.path)
        except OSError:
            return 0


def to_db(x):
    return 20 * math.log10(x) if x > 1e-6 else -120.0


def fmt_time(s):
    s = int(s)
    return f"{s // 3600:02d}:{s % 3600 // 60:02d}:{s % 60:02d}"


def fmt_span(s):
    """Coarse duration for estimates: '42 min', '3h 20m', '>99h'."""
    h, m = int(s // 3600), int(s % 3600 // 60)
    if h > 99:
        return ">99h"
    return f"~{h}h {m:02d}m" if h else f"~{max(m, 1)} min"


def fmt_size(n):
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024 or unit == "GB":
            return f"{n:.0f} {unit}" if unit == "B" else f"{n:.1f} {unit}"
        n /= 1024


def meter_bar(db, hold_db, width):
    def pos(d):
        return max(0, min(width, round((d - FLOOR_DB) / -FLOOR_DB * width)))

    filled, hold = pos(db), pos(hold_db)
    bar = Text()
    for i in range(width):
        d = FLOOR_DB + (i + 0.5) / width * -FLOOR_DB
        color = "green" if d < -18 else "yellow" if d < -6 else "red"
        if i < filled:
            bar.append("█", style=color)
        elif hold and i == hold - 1:
            bar.append("▏", style=f"bold {color}")
        else:
            bar.append("·", style="grey30")
    return bar


class Meters:
    """Smoothed level + peak-hold per channel."""

    def __init__(self, channels):
        self.level = [FLOOR_DB] * channels
        self.hold = [FLOOR_DB] * channels
        self.hold_at = [0.0] * channels
        self.last = time.monotonic()

    def update(self, peaks):
        now = time.monotonic()
        dt, self.last = now - self.last, now
        for i, p in enumerate(peaks):
            db = to_db(p)
            self.level[i] = max(db, self.level[i] - 45 * dt)
            if db >= self.hold[i]:
                self.hold[i], self.hold_at[i] = db, now
            elif now - self.hold_at[i] > 1.5:
                self.hold[i] = max(self.level[i], self.hold[i] - 30 * dt)


def channel_labels(n):
    return ["IN"] if n == 1 else ["L", "R"] if n == 2 else [str(i + 1) for i in range(n)]


def render(rec, meters, started):
    if rec.error:
        status = Text("■ ERROR", style="bold red")
    elif rec.paused:
        status = Text("❚❚ PAUSED", style="bold yellow")
    else:
        on = int((time.monotonic() - started) * 2) % 2 == 0
        status = Text("● REC" if on else "○ REC", style="bold red")

    head = Table.grid(expand=True)
    head.add_column(justify="left")
    head.add_column(justify="right")
    head.add_row(status, Text(fmt_time(rec.elapsed), style="bold white"))

    fmt = (f"FLAC · 24-bit · {rec.rate / 1000:g} kHz · "
           f"{'mono' if rec.channels == 1 else 'stereo' if rec.channels == 2 else f'{rec.channels} ch'}")
    info = Text()
    info.append(rec.dev["name"], style="bold")
    info.append(f"\n{fmt}", style="cyan")

    width = max(10, min(console.width, 100) - 34)
    rows = Table.grid(padding=(0, 1))
    for _ in range(4):
        rows.add_column()
    for i, label in enumerate(channel_labels(rec.channels)):
        hold = meters.hold[i]
        hold_txt = "-∞" if hold <= FLOOR_DB else f"{hold:+.1f}"
        style = "bold red" if hold > -1 else "dim"
        rows.add_row(Text(label, style="bold"),
                     meter_bar(meters.level[i], hold, width),
                     Text(f"{hold_txt:>5} dB", style=style), "")

    free = shutil.disk_usage(rec.path.parent).free
    stats = Text()
    stats.append(f"{fmt_size(rec.size)}", style="white")
    stats.append(f"  ·  {fmt_size(free)} free", style="dim")
    if rec.elapsed > 10 and rec.size > 0:
        stats.append(f" ({fmt_span(free / (rec.size / rec.elapsed))} left)", style="dim")
    stats.append("  ·  clips ", style="dim")
    stats.append(str(rec.clips), style="bold red" if rec.clips else "dim")
    lost = rec.dropped + rec.overruns
    stats.append("  ·  dropouts ", style="dim")
    stats.append(str(lost), style="bold red" if lost else "dim")

    parts = [head, Text(), info, Text(), rows, Text(), stats]
    if rec.note:
        parts += [Text(), Text(f"⚠ {rec.note}", style="yellow")]
    if rec.error:
        parts += [Text(), Text(f"⚠ write failed: {rec.error}", style="red")]
    elif rec.elapsed > 2 and not rec.heard_signal:
        parts += [Text(), Text("⚠ input is dead silent — check System Settings ▸ "
                               "Privacy & Security ▸ Microphone for your terminal", style="yellow")]
    parts += [Text(), Text("space pause/resume   q stop & save", style="dim")]

    return Panel(Group(*parts), title="[bold]rec[/]", title_align="left",
                 border_style="red" if (not rec.paused and not rec.error) else "yellow",
                 padding=(1, 2), width=min(console.width, 100))


class Keys:
    """Non-blocking single-key reads from the terminal."""

    def __enter__(self):
        self.fd = sys.stdin.fileno() if sys.stdin.isatty() else None
        if self.fd is not None:
            self.old = termios.tcgetattr(self.fd)
            tty.setcbreak(self.fd)  # ISIG stays on, so Ctrl+C still works
        return self

    def __exit__(self, *exc):
        if self.fd is not None:
            termios.tcsetattr(self.fd, termios.TCSADRAIN, self.old)

    def poll(self):
        if self.fd is not None and select.select([sys.stdin], [], [], 0)[0]:
            return sys.stdin.read(1)
        return None


def summary(rec, elapsed_wall):
    t = Table.grid(padding=(0, 2))
    t.add_column(style="dim")
    t.add_column()
    t.add_row("file", Text(str(rec.path), style="bold green", overflow="fold"))
    t.add_row("length", fmt_time(rec.elapsed) + f"  ({rec.frames:,} frames)")
    t.add_row("size", fmt_size(rec.size))
    t.add_row("format", f"FLAC · 24-bit · {rec.rate / 1000:g} kHz · {rec.channels} ch")
    if rec.clips:
        t.add_row("clips", Text(f"{rec.clips} blocks hit full scale", style="red"))
    if rec.dropped or rec.overruns:
        t.add_row("dropouts", Text(f"{rec.dropped + rec.overruns} (audio may have gaps)", style="red"))
    console.print(Panel(t, title="[bold]saved[/]", title_align="left",
                        border_style="green", padding=(1, 2), width=min(console.width, 100)))


def output_path(name, outdir):
    if name:
        p = Path(name).expanduser()
        if p.parent == Path("."):
            p = outdir / p
    else:
        p = outdir / f"rec-{datetime.now():%Y-%m-%d_%H-%M-%S}"
    p = p.with_suffix(".flac")
    p.parent.mkdir(parents=True, exist_ok=True)
    n = 2
    base = p
    while p.exists():
        p = base.with_name(f"{base.stem}-{n}.flac")
        n += 1
    return p


def main():
    ap = argparse.ArgumentParser(prog="rec", description="High-fidelity microphone recorder (24-bit FLAC).")
    ap.add_argument("name", nargs="?", help="file name (default: timestamp); saved in ~/Recordings")
    ap.add_argument("-d", "--device", help="input device: index or name substring (default: system input)")
    ap.add_argument("-r", "--rate", type=int, default=DEFAULT_RATE, help="sample rate in Hz (default: 96000)")
    ap.add_argument("-c", "--channels", type=int, help="channels (default: all, up to 2)")
    ap.add_argument("-o", "--outdir", type=Path, default=DEFAULT_DIR, help="output directory")
    ap.add_argument("-l", "--list", action="store_true", help="list input devices and exit")
    ap.add_argument("--resample", action="store_true",
                    help="let CoreAudio resample if the mic can't do the rate natively")
    args = ap.parse_args()

    if args.list:
        list_devices()
        return

    dev = pick_device(args.device)
    channels = args.channels or min(dev["max_input_channels"], 2)
    if channels > dev["max_input_channels"]:
        sys.exit(f"{dev['name']} only has {dev['max_input_channels']} input channel(s).")

    path = output_path(args.name, args.outdir.expanduser())
    try:
        rec = Recorder(dev, channels, args.rate, path, args.resample)
    except (sd.PortAudioError, OSError) as e:
        sys.exit(f"Could not start recording: {e}")

    meters = Meters(channels)
    started = time.monotonic()
    try:
        rec.start()
        with Keys() as keys, Live(render(rec, meters, started), console=console,
                                  refresh_per_second=FPS, transient=True) as live:
            while not rec.error:
                key = keys.poll()
                if key in ("q", "Q", "\x1b"):
                    break
                if key == " ":
                    rec.paused = not rec.paused
                meters.update(rec.take_peaks())
                live.update(render(rec, meters, started))
                time.sleep(1 / FPS)
    except KeyboardInterrupt:
        pass
    finally:
        rec.stop()

    if rec.error:
        console.print(f"[red]Write failed:[/] {rec.error}")
    summary(rec, time.monotonic() - started)


if __name__ == "__main__":
    main()
