//! Device selection, stream opening and the capture → disk pipeline.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, ErrorKind, FromSample, InputCallbackInfo, Sample, SampleFormat,
    SizedSample, Stream, StreamConfig, SupportedStreamConfigRange,
};
use rubato::{Fft, FixedSync, Resampler};

use crate::flac::FlacWriter;

pub const BLOCK: u32 = 4096; // frames per callback (~43 ms at 96 kHz)
const QUEUE_BLOCKS: usize = 4096; // ~30 min of slack if the disk stalls
const CLIP_LEVEL: f32 = 0.999;

pub struct InputDevice {
    pub index: usize,
    pub name: String,
    pub max_channels: u16,
    pub native_rate: u32,
    pub is_default: bool,
    pub device: Device,
    /// Queried once: each CoreAudio property read is comparatively slow.
    configs: Vec<SupportedStreamConfigRange>,
    default_format: SampleFormat,
}

fn describe(index: usize, device: Device, is_default: bool) -> Option<InputDevice> {
    let name = device.description().ok()?.name().to_string();
    let configs: Vec<_> = device.supported_input_configs().ok()?.collect();
    let max_channels = configs.iter().map(|c| c.channels()).max()?;
    let default = device.default_input_config().ok()?;
    Some(InputDevice {
        index,
        name,
        max_channels,
        native_rate: default.sample_rate(),
        is_default,
        device,
        configs,
        default_format: default.sample_format(),
    })
}

/// Every input device. Slow on machines with many devices, so only `-l` and `-d` use it.
pub fn input_devices() -> Vec<InputDevice> {
    let host = cpal::default_host();
    let default = host.default_input_device();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    devices
        .enumerate()
        .filter_map(|(i, d)| {
            let is_default = default.as_ref() == Some(&d);
            describe(i, d, is_default)
        })
        .collect()
}

/// Resolve `-d` (index or name substring) to a device.
pub fn pick_device(spec: Option<&str>) -> Result<InputDevice, String> {
    let Some(spec) = spec else {
        // Fast path: probe only the default device, not the whole device list.
        let host = cpal::default_host();
        if let Some(dev) = host
            .default_input_device()
            .and_then(|d| describe(0, d, true))
        {
            return Ok(dev);
        }
        return input_devices()
            .into_iter()
            .next()
            .ok_or_else(|| "No input devices found.".into());
    };
    let mut devs = input_devices();
    if devs.is_empty() {
        return Err("No input devices found.".into());
    }
    if let Ok(idx) = spec.parse::<usize>() {
        return match devs.iter().position(|d| d.index == idx) {
            Some(pos) => Ok(devs.swap_remove(pos)),
            None => Err(format!("No input device with index {spec}. Try: rec -l")),
        };
    }
    let needle = spec.to_lowercase();
    let mut matches: Vec<InputDevice> = devs
        .into_iter()
        .filter(|d| d.name.to_lowercase().contains(&needle))
        .collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(format!("No input device matching '{spec}'. Try: rec -l")),
        _ => {
            let names: Vec<String> = matches
                .iter()
                .map(|d| format!("{}:{}", d.index, d.name))
                .collect();
            Err(format!("'{spec}' is ambiguous: {}", names.join(", ")))
        }
    }
}

/// Counters and levels shared between the audio callback, the writer thread and the UI.
#[derive(Default)]
pub struct Shared {
    pub paused: AtomicBool,
    pub dropped: AtomicU64,  // blocks lost because the writer queue was full
    pub overruns: AtomicU64, // driver input overflows
    pub clips: AtomicU64,
    pub heard_signal: AtomicBool,
    pub frames: AtomicU64, // frames written to disk
    pub error: Mutex<Option<String>>,
    /// Per-channel peak since the UI last looked, as f32 bits (non-negative floats order like ints).
    peaks: Vec<AtomicU32>,
}

impl Shared {
    fn new(channels: usize) -> Self {
        Self {
            peaks: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            ..Default::default()
        }
    }

    pub fn take_peaks(&self) -> Vec<f32> {
        self.peaks
            .iter()
            .map(|p| f32::from_bits(p.swap(0, Relaxed)))
            .collect()
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    fn fail(&self, msg: String) {
        self.error.lock().unwrap().get_or_insert(msg);
    }
}

struct Opened {
    stream: Stream,
    /// Rate of the file we write.
    rate: u32,
    /// (device rate, file rate) when we have to convert in software.
    convert: Option<(u32, u32)>,
    note: Option<String>,
}

fn khz(rate: u32) -> String {
    format!("{}", rate as f64 / 1000.0)
}

/// Blocks flow callback → writer over `tx`; the writer hands emptied buffers back over
/// the pool so the real-time callback almost never allocates.
type Pool = Arc<Mutex<Vec<Vec<f32>>>>;

fn callback<T>(
    shared: Arc<Shared>,
    channels: usize,
    tx: SyncSender<Vec<f32>>,
    pool: Pool,
) -> impl FnMut(&[T], &InputCallbackInfo) + Send + 'static
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut peaks = vec![0f32; channels];
    move |data: &[T], _| {
        let recycled = pool.try_lock().ok().and_then(|mut p| p.pop());
        let mut samples = recycled.unwrap_or_default();
        samples.clear();
        samples.reserve(data.len());
        peaks.iter_mut().for_each(|p| *p = 0.0);
        for frame in data.chunks_exact(channels) {
            for (peak, &s) in peaks.iter_mut().zip(frame) {
                let v = f32::from_sample(s);
                *peak = peak.max(v.abs());
                samples.push(v);
            }
        }
        let mut block_peak = 0f32;
        for (slot, &peak) in shared.peaks.iter().zip(&peaks) {
            slot.fetch_max(peak.to_bits(), Relaxed);
            block_peak = block_peak.max(peak);
        }
        if block_peak > 0.0 {
            shared.heard_signal.store(true, Relaxed);
        }
        if shared.paused.load(Relaxed) {
            if let Ok(mut p) = pool.try_lock() {
                p.push(samples);
            }
            return;
        }
        if block_peak >= CLIP_LEVEL {
            shared.clips.fetch_add(1, Relaxed);
        }
        if let Err(TrySendError::Full(_)) = tx.try_send(samples) {
            shared.dropped.fetch_add(1, Relaxed);
        }
    }
}

fn build<T>(
    device: &Device,
    config: &StreamConfig,
    shared: &Arc<Shared>,
    tx: &SyncSender<Vec<f32>>,
    pool: &Pool,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let err_shared = shared.clone();
    let on_error = move |e: cpal::Error| match e.kind() {
        ErrorKind::Xrun => {
            err_shared.overruns.fetch_add(1, Relaxed);
        }
        ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied => {}
        _ => err_shared.fail(format!("audio stream error: {e}")),
    };
    let cb = callback::<T>(
        shared.clone(),
        config.channels as usize,
        tx.clone(),
        pool.clone(),
    );
    device.build_input_stream(*config, cb, on_error, None)
}

fn build_any(
    device: &Device,
    format: SampleFormat,
    config: &StreamConfig,
    shared: &Arc<Shared>,
    tx: &SyncSender<Vec<f32>>,
    pool: &Pool,
) -> Result<Stream, cpal::Error> {
    match format {
        SampleFormat::F32 => build::<f32>(device, config, shared, tx, pool),
        SampleFormat::I32 => build::<i32>(device, config, shared, tx, pool),
        SampleFormat::I16 => build::<i16>(device, config, shared, tx, pool),
        other => Err(cpal::Error::with_message(
            ErrorKind::UnsupportedConfig,
            format!("unsupported sample format {other}"),
        )),
    }
}

/// Open at `rate` if the hardware supports it natively. Otherwise fall back to the
/// device's native rate rather than silently resampling, unless `resample` is set.
fn open_stream(
    dev: &InputDevice,
    channels: u16,
    rate: u32,
    resample: bool,
    shared: &Arc<Shared>,
    tx: &SyncSender<Vec<f32>>,
    pool: &Pool,
) -> Result<Opened, cpal::Error> {
    let ranges: Vec<_> = dev
        .configs
        .iter()
        .filter(|c| c.channels() == channels)
        .collect();
    // Prefer f32 (what CoreAudio hands out); otherwise take whatever the device offers.
    let format = ranges
        .iter()
        .map(|c| c.sample_format())
        .find(|f| *f == SampleFormat::F32)
        .unwrap_or(dev.default_format);

    let attempt = |r: u32| -> Result<Stream, cpal::Error> {
        let mut config = StreamConfig {
            channels,
            sample_rate: r,
            buffer_size: BufferSize::Fixed(BLOCK),
        };
        build_any(&dev.device, format, &config, shared, tx, pool).or_else(|_| {
            config.buffer_size = BufferSize::Default;
            build_any(&dev.device, format, &config, shared, tx, pool)
        })
    };

    if ranges.iter().any(|c| c.contains_rate(rate)) {
        if let Ok(stream) = attempt(rate) {
            return Ok(Opened {
                stream,
                rate,
                convert: None,
                note: None,
            });
        }
    }
    let native = dev.native_rate;
    let stream = attempt(native)?;
    if native == rate {
        return Ok(Opened {
            stream,
            rate,
            convert: None,
            note: None,
        });
    }
    if resample {
        Ok(Opened {
            stream,
            rate,
            convert: Some((native, rate)),
            note: Some(format!(
                "resampled from {} to {} kHz in software",
                khz(native),
                khz(rate)
            )),
        })
    } else {
        Ok(Opened {
            stream,
            rate: native,
            convert: None,
            note: Some(format!(
                "device can't do {} kHz natively; recording at native {} kHz (use --resample to force)",
                khz(rate),
                khz(native)
            )),
        })
    }
}

/// Streaming fixed-ratio converter on interleaved f32.
struct Converter {
    resampler: Fft<f32>,
    channels: usize,
    input: Vec<f32>,
    output: Vec<f32>,
}

impl Converter {
    fn new(from: u32, to: u32, channels: usize) -> Result<Self, String> {
        let resampler = Fft::<f32>::new(
            from as usize,
            to as usize,
            BLOCK as usize,
            channels,
            FixedSync::Input,
        )
        .map_err(|e| format!("resampler: {e}"))?;
        let output = vec![0f32; resampler.output_frames_max() * channels];
        Ok(Self {
            resampler,
            channels,
            input: Vec::new(),
            output,
        })
    }

    fn process(
        &mut self,
        samples: &[f32],
        mut sink: impl FnMut(&[f32]) -> std::io::Result<()>,
    ) -> Result<(), String> {
        use audioadapter_buffers::direct::InterleavedSlice;
        self.input.extend_from_slice(samples);
        loop {
            let need = self.resampler.input_frames_next();
            if self.input.len() < need * self.channels {
                return Ok(());
            }
            let out_cap = self.output.len() / self.channels;
            let input =
                InterleavedSlice::new(&self.input[..need * self.channels], self.channels, need)
                    .map_err(|e| format!("resampler: {e}"))?;
            let mut output = InterleavedSlice::new_mut(&mut self.output, self.channels, out_cap)
                .map_err(|e| format!("resampler: {e}"))?;
            let (_, produced) = self
                .resampler
                .process_into_buffer(&input, &mut output, None)
                .map_err(|e| format!("resampler: {e}"))?;
            self.input.drain(..need * self.channels);
            sink(&self.output[..produced * self.channels]).map_err(|e| e.to_string())?;
        }
    }
}

pub struct Recorder {
    pub device_name: String,
    pub channels: u16,
    pub rate: u32,
    pub path: PathBuf,
    pub note: Option<String>,
    pub shared: Arc<Shared>,
    stream: Option<Stream>,
    writer: Option<JoinHandle<()>>,
}

impl Recorder {
    pub fn new(
        dev: &InputDevice,
        channels: u16,
        rate: u32,
        path: &Path,
        resample: bool,
    ) -> Result<Self, String> {
        let shared = Arc::new(Shared::new(channels as usize));
        let (tx, rx) = sync_channel::<Vec<f32>>(QUEUE_BLOCKS);
        let pool: Pool = Arc::default();
        let opened = open_stream(dev, channels, rate, resample, &shared, &tx, &pool)
            .map_err(|e| e.to_string())?;
        drop(tx);
        let file =
            FlacWriter::create(path, opened.rate, channels as usize).map_err(|e| e.to_string())?;
        let converter = match opened.convert {
            Some((from, to)) => Some(Converter::new(from, to, channels as usize)?),
            None => None,
        };
        let writer = {
            let shared = shared.clone();
            let pool = pool.clone();
            std::thread::spawn(move || {
                write_loop(rx, file, converter, channels as usize, shared, pool)
            })
        };
        Ok(Self {
            device_name: dev.name.clone(),
            channels,
            rate: opened.rate,
            path: path.to_path_buf(),
            note: opened.note,
            shared,
            stream: Some(opened.stream),
            writer: Some(writer),
        })
    }

    pub fn start(&self) -> Result<(), String> {
        match &self.stream {
            Some(stream) => stream.play().map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// Stop capture, drain the queue to disk and finalize the file.
    pub fn stop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
            // Dropping the stream drops the callback's sender, which ends the writer loop.
            drop(stream);
        }
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
    }

    pub fn live_state(&self) -> crate::ui::LiveState {
        let c = &self.shared;
        crate::ui::LiveState {
            device_name: self.device_name.clone(),
            rate: self.rate,
            channels: self.channels,
            elapsed: self.elapsed(),
            size: self.size(),
            paused: self.paused(),
            clips: c.clips.load(Relaxed),
            dropouts: c.dropped.load(Relaxed) + c.overruns.load(Relaxed),
            heard_signal: c.heard_signal.load(Relaxed),
            note: self.note.clone(),
            error: c.error(),
        }
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Relaxed)
    }

    pub fn toggle_pause(&self) {
        self.shared.paused.fetch_xor(true, Relaxed);
    }

    pub fn frames(&self) -> u64 {
        self.shared.frames.load(Relaxed)
    }

    pub fn elapsed(&self) -> f64 {
        self.frames() as f64 / self.rate as f64
    }

    pub fn size(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }
}

fn write_loop(
    rx: Receiver<Vec<f32>>,
    mut file: FlacWriter,
    mut converter: Option<Converter>,
    channels: usize,
    shared: Arc<Shared>,
    pool: Pool,
) {
    let mut result = Ok(());
    for block in rx {
        let mut write = |s: &[f32]| -> std::io::Result<()> {
            file.push(s)?;
            shared
                .frames
                .fetch_add((s.len() / channels) as u64, Relaxed);
            Ok(())
        };
        result = match converter.as_mut() {
            Some(c) => c.process(&block, &mut write),
            None => write(&block).map_err(|e| e.to_string()),
        };
        if let Ok(mut p) = pool.lock() {
            if p.len() < 64 {
                p.push(block);
            }
        }
        if result.is_err() {
            break; // disk full, unplugged volume, ...
        }
    }
    if let Err(e) = result {
        shared.fail(e);
    }
    if let Err(e) = file.finish() {
        shared.fail(e.to_string());
    }
}
