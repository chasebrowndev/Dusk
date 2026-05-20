use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
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

pub fn start(net_tx: mpsc::Sender<ClientMsg>) -> Result<VoiceHandle> {
    let host = cpal::default_host();

    let in_dev = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("no microphone found"))?;
    let out_dev = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no audio output found"))?;

    let cfg = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    // ---- Capture ----
    // cpal callback -> std channel -> encoding thread -> blocking_send to net_tx
    let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(32);

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
        let mut opus_out = vec![0u8; 512];

        loop {
            match pcm_rx.recv_timeout(std::time::Duration::from_millis(20)) {
                Ok(chunk) => {
                    acc.extend_from_slice(&chunk);
                    while acc.len() >= FRAME_SAMPLES {
                        for (i, &s) in acc[..FRAME_SAMPLES].iter().enumerate() {
                            pcm_i16[i] = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }
                        acc.drain(..FRAME_SAMPLES);
                        if let Ok(n) = encoder.encode(&pcm_i16, &mut opus_out) {
                            let data = STANDARD.encode(&opus_out[..n]);
                            let _ = net_tx.blocking_send(ClientMsg::VoiceData { data });
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
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

    // Decoding thread: receives encoded frames, decodes, fills playback buffer
    let (frame_in, frame_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);

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
