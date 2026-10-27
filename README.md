# evidence

A handy suite of small tools to collect evidence in various scenarios.

## Tools

### rec

A terminal microphone recorder that writes 24-bit FLAC at 96 kHz by default. It shows a live level meter with peak hold, a clock, file size, free disk space with a time-left estimate, and counters for clipped blocks and dropouts.

Written in Rust: a single self-contained binary that starts in a few milliseconds. Source: [`rec/`](rec/)

#### Requirements

- A microphone (any input device the OS exposes through [cpal](https://crates.io/crates/cpal))
- macOS is the main target (CoreAudio). On Linux it uses ALSA and needs `libasound2-dev` to build. It needs a terminal, and it does not run on Windows.
- A Rust toolchain to build it (stable, see [rustup.rs](https://rustup.rs)). There are no runtime dependencies. FLAC encoding is built in.

#### Build

```sh
cd rec
cargo build --release          # binary at rec/target/release/rec
cargo install --path .         # or install it onto your PATH
```

CI cross-compiles for Intel Macs (`x86_64-apple-darwin`) on every change to `rec/` and uploads the binary as a build artifact. To do the same locally on a Mac:

```sh
rustup target add x86_64-apple-darwin
cargo build --release --target x86_64-apple-darwin
```

#### Usage

```sh
rec                     # record to ~/Recordings/rec-YYYY-MM-DD_HH-MM-SS.flac
rec interview           # record to ~/Recordings/interview.flac
rec ./take1.flac        # a path with a directory goes exactly there
rec -l                  # list input devices and exit
rec -d "USB" -c 1       # pick a device by index or name substring, mono
```

| Option | Meaning |
| --- | --- |
| `name` | File name. Saved in the output directory unless it includes a directory. `.flac` is always the extension. |
| `-d, --device` | Input device, as an index from `-l` or a name substring. Defaults to the system input. |
| `-r, --rate` | Sample rate in Hz. Default `96000`. |
| `-c, --channels` | Channel count. Default is all channels, up to 2. |
| `-o, --outdir` | Output directory. Default `~/Recordings`. |
| `-l, --list` | List input devices and exit. |
| `--resample` | Record at the device's native rate and convert to the requested rate in software, if the device cannot run it natively. |

Existing files are never overwritten. If the name is taken, the new file gets a suffix such as `-2`.

#### Controls

- `space`: pause or resume. Paused audio is not written.
- `q`, `Q`, or `Esc`: stop and save.
- `Ctrl+C`: also stops and saves.

#### Behaviour worth knowing

- **Sample rate.** If the device cannot run the requested rate natively, `rec` records at the device's native rate and shows a warning. It does not resample unless you pass `--resample`, which converts in software (FFT resampler). A tail shorter than one resampler block (about 43 ms) is dropped when the recording stops.
- **Clips and dropouts.** Blocks that reach full scale are counted as clips. Blocks lost because the disk or writer fell behind, or the driver overflowed, are counted as dropouts. The summary at the end reports both, and dropouts mean the audio may have gaps.
- **Crash safety.** The FLAC header is written up front and finalized (length, checksum) when you stop, so an interrupted recording is still playable.
- **Silent input.** If nothing is heard after a couple of seconds, `rec` warns you. On macOS, check System Settings ▸ Privacy & Security ▸ Microphone for your terminal app.
- **Write errors.** If the disk fills or the volume disappears, recording stops and the error is shown. The file keeps what was written before the failure.

## License

Licensed under the GNU Affero General Public License v3.0. See [LICENSE](LICENSE).
