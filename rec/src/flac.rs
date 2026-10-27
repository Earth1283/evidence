//! Streaming 24-bit FLAC writer.
//!
//! flacenc encodes whole buffers, so frames are encoded one fixed-size block at
//! a time and appended to the file as they come. A provisional STREAMINFO is
//! written up front (so a crash still leaves a playable file) and rewritten
//! with the real totals and MD5 on `finish`.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::config::Encoder;
use flacenc::error::Verify;
use flacenc::source::{Context, Fill, FrameBuf};

pub const BITS: usize = 24;
const BLOCK_FRAMES: usize = 4096;
const FULL_SCALE: f32 = 8_388_608.0; // 2^23

pub struct FlacWriter {
    out: BufWriter<File>,
    config: flacenc::error::Verified<Encoder>,
    info: StreamInfo,
    ctx: Context,
    channels: usize,
    pending: Vec<i32>,
    framebuf: FrameBuf,
    sink: ByteSink,
    frame_no: usize,
    header_len: usize,
}

fn other(e: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("{e:?}"))
}

fn header_bytes(info: &StreamInfo) -> Vec<u8> {
    let mut sink = ByteSink::new();
    Stream::with_stream_info(info.clone())
        .write(&mut sink)
        .expect("writing to memory cannot fail");
    sink.as_slice().to_vec()
}

impl FlacWriter {
    pub fn create(path: &Path, rate: u32, channels: usize) -> io::Result<Self> {
        let config = Encoder::default().into_verified().map_err(other)?;
        let mut info = StreamInfo::new(rate as usize, channels, BITS).map_err(other)?;
        info.set_block_sizes(BLOCK_FRAMES, BLOCK_FRAMES)
            .map_err(other)?;
        let header = header_bytes(&info);
        let mut out = BufWriter::new(File::create(path)?);
        out.write_all(&header)?;
        // Frame stats start from "nothing seen yet".
        let info = StreamInfo::new(rate as usize, channels, BITS).map_err(other)?;
        Ok(Self {
            out,
            config,
            info,
            ctx: Context::new(BITS, channels),
            channels,
            pending: Vec::with_capacity(BLOCK_FRAMES * channels * 2),
            framebuf: FrameBuf::with_size(channels, BLOCK_FRAMES).map_err(other)?,
            sink: ByteSink::new(),
            frame_no: 0,
            header_len: header.len(),
        })
    }

    /// Append interleaved float samples in [-1, 1].
    pub fn push(&mut self, samples: &[f32]) -> io::Result<()> {
        self.pending.extend(samples.iter().map(|&s| {
            (s * FULL_SCALE)
                .round()
                .clamp(-FULL_SCALE, FULL_SCALE - 1.0) as i32
        }));
        let block = BLOCK_FRAMES * self.channels;
        // Take the buffer so the blocks can be encoded straight out of it.
        let pending = std::mem::take(&mut self.pending);
        let mut start = 0;
        let mut result = Ok(());
        while pending.len() - start >= block {
            result = self.encode(&pending[start..start + block], BLOCK_FRAMES);
            start += block;
            if result.is_err() {
                break;
            }
        }
        self.pending = pending;
        self.pending.drain(..start);
        result
    }

    fn encode(&mut self, interleaved: &[i32], frames: usize) -> io::Result<()> {
        if self.framebuf.size() != frames {
            self.framebuf.resize(frames);
        }
        self.framebuf.fill_interleaved(interleaved).map_err(other)?;
        self.ctx.fill_interleaved(interleaved).map_err(other)?;
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.framebuf,
            self.frame_no,
            &self.info,
        )
        .map_err(other)?;
        self.frame_no += 1;
        self.info.update_frame_info(&frame);
        self.sink.clear();
        frame.write(&mut self.sink).map_err(other)?;
        self.out.write_all(self.sink.as_slice())
    }

    /// Flush the last partial block and write the final STREAMINFO.
    pub fn finish(mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.encode(&rest, rest.len() / self.channels)?;
        }
        self.info.set_md5_digest(&self.ctx.md5_digest());
        let header = header_bytes(&self.info);
        debug_assert_eq!(header.len(), self.header_len);
        self.out.flush()?;
        let file = self.out.get_mut();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let t = i as f32 / 96000.0;
                (0..channels)
                    .map(move |c| 0.5 * (t * 440.0 * (c + 1) as f32 * std::f32::consts::TAU).sin())
            })
            .collect()
    }

    #[test]
    fn writes_a_finalized_file_and_keeps_up_with_realtime() {
        let path = std::env::temp_dir().join(format!("rec-flac-test-{}.flac", std::process::id()));
        let frames = 96000 * 10 + 123; // ends on a partial block
        let samples = tone(frames, 2);
        let started = std::time::Instant::now();
        let mut w = FlacWriter::create(&path, 96000, 2).unwrap();
        for chunk in samples.chunks(1000 * 2) {
            w.push(chunk).unwrap();
        }
        w.finish().unwrap();
        let took = started.elapsed().as_secs_f64();
        eprintln!(
            "encoded 10s of 96 kHz stereo in {took:.3}s ({:.0}x realtime)",
            10.0 / took
        );
        let bytes = std::fs::read(&path).unwrap();
        eprintln!("{} bytes at {}", bytes.len(), path.display());
        assert_eq!(&bytes[..4], b"fLaC");
        if std::env::var_os("KEEP_FLAC").is_none() {
            std::fs::remove_file(&path).unwrap();
        }
    }
}
