use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use cpal::traits::{HostTrait, StreamTrait};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::protocol::ClientMsg;

const SAMPLE_RATE: u32 = 48000;
const FRAME_SAMPLES: usize = 480; // 10ms at 48kHz
// Hard ceiling per speaker: if a speaker's jitter buffer grows past this,
// drop the oldest samples to stay current.
const JITTER_MAX: usize = FRAME_SAMPLES * 4; // 40ms

pub struct VoiceHandle {
    /// Send (nick, base64-decoded Opus bytes) for each incoming VoiceFrame.
    /// Each nick gets its own decoder and buffer; outputs are mixed at playback.
    pub frame_in: std::sync::mpsc::SyncSender<(String, Vec<u8>)>,
    _input_stream: cpal::Stream,
    _output_stream: cpal::Stream,
}

pub fn start(
    net_tx: mpsc::Sender<ClientMsg>,
    audio_in: Option<&str>,
    audio_out: Option<&str>,
) -> Result<VoiceHandle> {
    use cpal::traits::DeviceTrait;
    let host = cpal::default_host();

    // Saved names from prior runs may point at ALSA plugin stubs (lavrate,
    // samplerate, …) or devices that have since disappeared. Treat them as
    // hints: if the lookup misses, fall back to the host default rather than
    // failing voice entirely.
    let in_dev = audio_in
        .and_then(|name| {
            host.input_devices()
                .ok()
                .and_then(|mut it| it.find(|d| d.name().ok().as_deref() == Some(name)))
        })
        // Prefer any real mic over a monitor source. On PipeWire/ALSA, monitor
        // sources (which capture playback) show up as input devices and cause echo
        // if selected as the default. Fall back to the system default only if no
        // non-monitor input exists.
        .or_else(|| {
            host.input_devices().ok()?.find(|d| {
                let name = d.name().unwrap_or_default();
                !name.to_lowercase().contains("monitor")
            })
        })
        .or_else(|| host.default_input_device())
        .ok_or_else(|| anyhow::anyhow!("no microphone found"))?;
    let out_dev = audio_out
        .and_then(|name| {
            host.output_devices()
                .ok()
                .and_then(|mut it| it.find(|d| d.name().ok().as_deref() == Some(name)))
        })
        .or_else(|| host.default_output_device())
        .ok_or_else(|| anyhow::anyhow!("no audio output found"))?;

    let cfg = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        // Request one Opus frame per cpal callback (10ms). ALSA may not honor
        // this exactly — the accumulator in the encoder handles misalignment.
        buffer_size: cpal::BufferSize::Fixed(FRAME_SAMPLES as u32),
    };

    // ---- Capture ----
    // cpal callback -> std channel -> encoding thread -> try_send to net_tx
    // 2 frames of slack; if the encoder falls behind we drop rather than queue.
    let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(2);

    let input_stream = in_dev.build_input_stream(
        &cfg,
        move |data: &[f32], _| {
            let _ = pcm_tx.try_send(data.to_vec());
        },
        |e| eprintln!("mic error: {e}"),
        None,
    )?;

    // Encoding thread (std::thread so opus::Encoder doesn't need Send)
    std::thread::spawn(move || {
        // LowDelay minimises codec algorithmic latency (no lookahead).
        let mut encoder = match opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::LowDelay) {
            Ok(e) => e,
            Err(e) => { eprintln!("opus encoder init: {e}"); return; }
        };
        let _ = encoder.set_bitrate(opus::Bitrate::Bits(32000));

        let mut acc: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 2);
        let mut pcm_i16 = vec![0i16; FRAME_SAMPLES];
        let mut opus_out = vec![0u8; 1276];

        loop {
            // Block with a timeout so a stalled/unplugged device doesn't park
            // this thread forever. On timeout there's nothing to encode; we
            // just loop back and wait again.
            match pcm_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(chunk) => acc.extend_from_slice(&chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            }
            // Drain any additional chunks that arrived while we were encoding.
            while let Ok(chunk) = pcm_rx.try_recv() {
                acc.extend_from_slice(&chunk);
            }

            while acc.len() >= FRAME_SAMPLES {
                for (i, &s) in acc[..FRAME_SAMPLES].iter().enumerate() {
                    pcm_i16[i] = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                }
                acc.drain(..FRAME_SAMPLES);
                if let Ok(n) = encoder.encode(&pcm_i16, &mut opus_out) {
                    let data = STANDARD.encode(&opus_out[..n]);
                    // Drop the frame if net is congested — stale audio is worse than a gap.
                    let _ = net_tx.try_send(ClientMsg::VoiceData { data });
                }
            }
        }
    });

    // ---- Playback ----
    // Per-speaker PCM queues. The output callback sums across all active
    // speakers sample-by-sample (proper mixing), rather than interleaving
    // their frames sequentially.
    let mix_bufs: Arc<Mutex<HashMap<String, VecDeque<f32>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mix_read = mix_bufs.clone();
    let mix_write = mix_bufs.clone();

    let output_stream = out_dev.build_output_stream(
        &cfg,
        move |data: &mut [f32], _| {
            let mut bufs = mix_read.lock().unwrap();
            for s in data.iter_mut() {
                let mut sum = 0.0f32;
                for buf in bufs.values_mut() {
                    sum += buf.pop_front().unwrap_or(0.0);
                }
                // Clamp after summing to prevent digital clipping when multiple
                // speakers are simultaneously near full amplitude.
                *s = sum.clamp(-1.0, 1.0);
            }
        },
        |e| eprintln!("playback error: {e}"),
        None,
    )?;

    input_stream.play()?;
    output_stream.play()?;

    // Decoding thread: one stateful Opus decoder per remote speaker, so that
    // inter-frame prediction is never corrupted by interleaving streams.
    // 2 frames of slack; extras are dropped by try_send in the UI event loop.
    let (frame_in, frame_rx) = std::sync::mpsc::sync_channel::<(String, Vec<u8>)>(2);

    std::thread::spawn(move || {
        let mut decoders: HashMap<String, opus::Decoder> = HashMap::new();
        let mut pcm_i16 = vec![0i16; FRAME_SAMPLES * 4];

        while let Ok((nick, data)) = frame_rx.recv() {
            let decoder = match decoders.entry(nick.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) {
                        Ok(d) => e.insert(d),
                        Err(err) => { eprintln!("opus decoder init for {nick}: {err}"); continue; }
                    }
                }
            };

            if let Ok(n) = decoder.decode(&data, &mut pcm_i16, false) {
                let samples = pcm_i16[..n].iter().map(|&s| s as f32 / 32767.0);
                let mut bufs = mix_write.lock().unwrap();
                let buf = bufs.entry(nick).or_default();
                buf.extend(samples);
                if buf.len() > JITTER_MAX {
                    let excess = buf.len() - JITTER_MAX;
                    buf.drain(..excess);
                }
            }
        }
    });

    Ok(VoiceHandle {
        frame_in,
        _input_stream: input_stream,
        _output_stream: output_stream,
    })
}
