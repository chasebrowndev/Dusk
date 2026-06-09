// PipeWire/PulseAudio audio isolation for screen-share.
//
// When sharing, Dusk's own voice-chat output would appear in @DEFAULT_MONITOR@
// and get re-transmitted, causing viewers to hear their own voices back.
// AudioIsolation fixes this by:
//   1. Creating a silent null sink ("dusk_share_priv")
//   2. Routing Dusk's sink inputs to it (found by application.name = "dusk")
//   3. Adding a loopback from that sink's monitor → real output so the local
//      user still hears the voice call
//   4. Capturing @DEFAULT_MONITOR@ which no longer contains Dusk audio
//
// Everything is restored automatically when the struct is dropped.
// If pactl is unavailable the struct is a no-op — best-effort only.

use std::process::Command;

pub struct AudioIsolation {
    null_module: Option<u32>,
    loopback_module: Option<u32>,
    moved: Vec<(u32, u32)>, // (sink_input_id, original_sink_id)
}

impl AudioIsolation {
    pub fn setup() -> Self {
        let mut s = Self { null_module: None, loopback_module: None, moved: Vec::new() };

        let null_out = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                "sink_name=dusk_share_priv",
                "sink_properties=device.description=Dusk-share-isolate",
            ])
            .output();
        let Ok(null_out) = null_out else { return s };
        s.null_module = String::from_utf8_lossy(&null_out.stdout).trim().parse().ok();

        if s.null_module.is_none() {
            return s;
        }

        // Loopback: null sink monitor → real output so the user still hears
        // voice chat while the monitor capture stays clean.
        if let Ok(lb) = Command::new("pactl")
            .args([
                "load-module",
                "module-loopback",
                "source=dusk_share_priv.monitor",
                "latency_msec=10",
            ])
            .output()
        {
            s.loopback_module = String::from_utf8_lossy(&lb.stdout).trim().parse().ok();
        }

        for (input_id, orig_sink) in dusk_sink_inputs() {
            let moved = Command::new("pactl")
                .args(["move-sink-input", &input_id.to_string(), "dusk_share_priv"])
                .status()
                .map(|st| st.success())
                .unwrap_or(false);
            if moved {
                s.moved.push((input_id, orig_sink));
            }
        }

        s
    }

    fn teardown(&mut self) {
        for (input_id, orig) in self.moved.drain(..) {
            let _ = Command::new("pactl")
                .args(["move-sink-input", &input_id.to_string(), &orig.to_string()])
                .status();
        }
        // Unload loopback before null sink — loopback reads from its monitor.
        for id in [self.loopback_module.take(), self.null_module.take()]
            .into_iter()
            .flatten()
        {
            let _ = Command::new("pactl")
                .args(["unload-module", &id.to_string()])
                .status();
        }
    }
}

impl Drop for AudioIsolation {
    fn drop(&mut self) {
        self.teardown();
    }
}

// Parse `pactl list sink-inputs` and return (sink_input_id, current_sink_id)
// for every input whose application.name contains "dusk".
fn dusk_sink_inputs() -> Vec<(u32, u32)> {
    let Ok(out) = Command::new("pactl").args(["list", "sink-inputs"]).output() else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&out.stdout);

    let mut result = Vec::new();
    let mut cur_input: Option<u32> = None;
    let mut cur_sink: Option<u32> = None;
    let mut is_dusk = false;

    for line in text.lines() {
        let t = line.trim();
        if let Some(n) = t.strip_prefix("Sink Input #") {
            // Flush previous block before starting a new one.
            if is_dusk {
                if let (Some(i), Some(s)) = (cur_input, cur_sink) {
                    result.push((i, s));
                }
            }
            cur_input = n.parse().ok();
            cur_sink = None;
            is_dusk = false;
        } else if let Some(n) = t.strip_prefix("Sink: ") {
            cur_sink = n.trim().parse().ok();
        } else if t.contains("application.name") && t.contains("\"dusk\"") {
            is_dusk = true;
        }
    }
    // Flush the last block.
    if is_dusk {
        if let (Some(i), Some(s)) = (cur_input, cur_sink) {
            result.push((i, s));
        }
    }
    result
}
