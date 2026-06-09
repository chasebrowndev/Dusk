use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use cpal::traits::{HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::protocol::ClientMsg;

const SAMPLE_RATE: u32 = 48000;
const FRAME_SAMPLES: usize = 960; // 20ms at 48kHz
const MAX_PLAY_SAMPLES: usize = SAMPLE_RATE as usize; // 1s jitter buffer cap

pub struct VoiceHandle {
    /// Send base64-decoded Opus frames received from the server here to play them.
    pub frame_in: std::sync::mpsc::SyncSender<Vec<u8>>,
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
        // Fix buffer size to one Opus frame (20ms) to minimize capture latency.
        // Default lets the OS choose, which is often 512–4096 samples (10–85ms).
        buffer_size: cpal::BufferSize::Fixed(FRAME_SAMPLES as u32),
    };

    // ---- Capture ----
    // cpal callback -> std channel -> encoding thread -> blocking_send to net_tx
    // Small channel: 4 frames = 80ms max queuing before backpressure.
    let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(4);

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
        let mut encoder = match opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip) {
            Ok(e) => e,
            Err(e) => { eprintln!("opus encoder init: {e}"); return; }
        };
        let _ = encoder.set_bitrate(opus::Bitrate::Bits(32000));

        let mut acc: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 2);
        let mut pcm_i16 = vec![0i16; FRAME_SAMPLES];
        let mut opus_out = vec![0u8; 1276]; // libopus minimum recommended size

        loop {
            // Drain ALL available PCM chunks before encoding to keep the
            // accumulator current. Sleep 1ms only when the channel is empty
            // and we don't yet have a full frame.
            loop {
                match pcm_rx.try_recv() {
                    Ok(chunk) => acc.extend_from_slice(&chunk),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                }
            }
            if acc.len() < FRAME_SAMPLES {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            while acc.len() >= FRAME_SAMPLES {
                for (i, &s) in acc[..FRAME_SAMPLES].iter().enumerate() {
                    pcm_i16[i] = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                }
                acc.drain(..FRAME_SAMPLES);
                if let Ok(n) = encoder.encode(&pcm_i16, &mut opus_out) {
                    let data = STANDARD.encode(&opus_out[..n]);
                    let _ = net_tx.blocking_send(ClientMsg::VoiceData { data });
                }
            }
        }
    });

    // ---- Playback ----
    let play_buf: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
    let play_read = play_buf.clone();
    let play_write = play_buf.clone();

    let output_stream = out_dev.build_output_stream(
        &cfg,
        move |data: &mut [f32], _| {
            let mut buf = play_read.lock().unwrap();
            for s in data.iter_mut() {
                *s = buf.pop_front().unwrap_or(0.0);
            }
        },
        |e| eprintln!("playback error: {e}"),
        None,
    )?;

    input_stream.play()?;
    output_stream.play()?;

    // Decoding thread: receives encoded frames, decodes, fills playback buffer.
    // Small channel: 4 frames = 80ms max queuing before backpressure.
    let (frame_in, frame_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);

    std::thread::spawn(move || {
        let mut decoder = match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) {
            Ok(d) => d,
            Err(e) => { eprintln!("opus decoder init: {e}"); return; }
        };

        let mut pcm_i16 = vec![0i16; FRAME_SAMPLES * 2];

        while let Ok(data) = frame_rx.recv() {
            if let Ok(n) = decoder.decode(&data, &mut pcm_i16, false) {
                let samples: Vec<f32> = pcm_i16[..n]
                    .iter()
                    .map(|&s| s as f32 / 32767.0)
                    .collect();
                let mut buf = play_write.lock().unwrap();
                if buf.len() < MAX_PLAY_SAMPLES {
                    buf.extend(samples);
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
