# evidence

A handy suite of Python scripts to collect evidence in various scenarios.

## Tools

### rec

A terminal microphone recorder that writes 24-bit FLAC at 96 kHz by default. It shows a live level meter with peak hold, a clock, file size, free disk space with a time-left estimate, and counters for clipped blocks and dropouts.

Source: [`rec/main.py`](rec/main.py)

#### Requirements

- Python 3
- A microphone (any input device PortAudio can see)
- macOS is the main target. Rate switching and `--resample` use CoreAudio and are macOS-only. Elsewhere it runs through PortAudio with the device's native rate. It needs a POSIX terminal (it uses `termios`), so it does not run on Windows.
- PortAudio and libsndfile. The `sounddevice` and `soundfile` wheels bundle them on macOS and Windows. On Linux you may need to install `libportaudio2` and `libsndfile1` from your package manager.

#### Install

```sh
python3 -m venv .venv
source .venv/bin/activate
pip install -r rec/requirements.txt
```

#### Usage

```sh
python3 rec/main.py                     # record to ~/Recordings/rec-YYYY-MM-DD_HH-MM-SS.flac
python3 rec/main.py interview           # record to ~/Recordings/interview.flac
python3 rec/main.py ./take1.flac        # a path with a directory goes exactly there
python3 rec/main.py -l                  # list input devices and exit
python3 rec/main.py -d "USB" -c 1       # pick a device by index or name substring, mono
```

| Option | Meaning |
| --- | --- |
| `name` | File name. Saved in the output directory unless it includes a directory. `.flac` is always the extension. |
| `-d, --device` | Input device, as an index from `-l` or a name substring. Defaults to the system input. |
| `-r, --rate` | Sample rate in Hz. Default `96000`. |
| `-c, --channels` | Channel count. Default is all channels, up to 2. |
| `-o, --outdir` | Output directory. Default `~/Recordings`. |
| `-l, --list` | List input devices and exit. |
| `--resample` | Let CoreAudio resample if the device cannot run the requested rate natively. |

Existing files are never overwritten. If the name is taken, the new file gets a suffix such as `-2`.

#### Controls

- `space`: pause or resume. Paused audio is not written.
- `q`, `Q`, or `Esc`: stop and save.
- `Ctrl+C`: also stops and saves.

#### Behaviour worth knowing

- **Sample rate.** If the device cannot run the requested rate natively, `rec` records at the device's native rate and shows a warning. It does not resample unless you pass `--resample`.
- **Clips and dropouts.** Blocks that reach full scale are counted as clips. Blocks lost because the disk or writer fell behind, or the driver overflowed, are counted as dropouts. The summary at the end reports both, and dropouts mean the audio may have gaps.
- **Silent input.** If nothing is heard after a couple of seconds, `rec` warns you. On macOS, check System Settings ▸ Privacy & Security ▸ Microphone for your terminal app.
- **Write errors.** If the disk fills or the volume disappears, recording stops and the error is shown. The file keeps what was written before the failure.

## License

Licensed under the GNU Affero General Public License v3.0. See [LICENSE](LICENSE).
