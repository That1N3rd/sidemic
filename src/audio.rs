//! Device lookup (cpal) and two small linear resamplers.

use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::Device;

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

pub fn find_capture(filter: &str) -> Result<Device> {
    let host = cpal::default_host();
    if filter.trim().is_empty() {
        return host
            .default_input_device()
            .ok_or_else(|| anyhow!("No default microphone. Plug one in or set micDevice in sidemic.json."));
    }
    host.input_devices()?
        .find(|d| d.name().map(|n| contains_ci(&n, filter)).unwrap_or(false))
        .ok_or_else(|| anyhow!("No microphone matches \"{filter}\". Run: sidemic devices"))
}

/// Host side: the playback endpoint whose audio comes back out of a virtual microphone.
/// Auto order: VB-Cable ("CABLE Input"), then Steam Streaming Microphone ("Speakers (Steam Streaming Microphone)").
pub fn find_virtual_mic_output(filter: &str) -> Result<Device> {
    let host = cpal::default_host();
    let devices: Vec<Device> = host.output_devices()?.collect();
    let name_of = |d: &Device| d.name().unwrap_or_default();
    if !filter.trim().is_empty() {
        return devices
            .into_iter()
            .find(|d| contains_ci(&name_of(d), filter))
            .ok_or_else(|| anyhow!("No active playback device matches \"{filter}\". Run: sidemic devices"));
    }
    for pref in ["CABLE Input", "Steam Streaming Microphone"] {
        if let Some(d) = devices.iter().find(|d| contains_ci(&name_of(d), pref)) {
            return Ok(d.clone());
        }
    }
    Err(anyhow!(
        "No virtual microphone found. Install VB-Cable (vb-audio.com/Cable), or enable \"Speakers (Steam Streaming Microphone)\" \
         in mmsys.cpl > Playback (right-click > Show Disabled Devices) and the matching Recording device, then restart sidemic. \
         Or set outputDevice in sidemic.json."
    ))
}

pub fn describe() -> String {
    let host = cpal::default_host();
    let mut out = String::new();
    let default_in = host.default_input_device().and_then(|d| d.name().ok());
    let default_out = host.default_output_device().and_then(|d| d.name().ok());
    out.push_str("Microphones (capture):\n");
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            let n = d.name().unwrap_or_else(|_| "?".into());
            let mark = if Some(&n) == default_in.as_ref() { " (default)" } else { "" };
            let fmt = d
                .default_input_config()
                .map(|c| format!("{} Hz, {} ch", c.sample_rate().0, c.channels()))
                .unwrap_or_default();
            out.push_str(&format!("  {n}{mark}  [{fmt}]\n"));
        }
    }
    out.push_str("Playback (render):\n");
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            let n = d.name().unwrap_or_else(|_| "?".into());
            let mark = if Some(&n) == default_out.as_ref() { " (default)" } else { "" };
            let fmt = d
                .default_output_config()
                .map(|c| format!("{} Hz, {} ch", c.sample_rate().0, c.channels()))
                .unwrap_or_default();
            out.push_str(&format!("  {n}{mark}  [{fmt}]\n"));
        }
    }
    out.push_str("(Only enabled devices are listed. Disabled ones show up in mmsys.cpl > Show Disabled Devices.)\n");
    out
}

/// Push-model linear resampler: feed blocks of mono samples at `from` Hz, get mono samples at `to` Hz.
pub struct PushResampler {
    ratio: f64, // input samples per output sample
    pos: f64,   // fractional read position; 0 == `prev`
    prev: f32,
    bypass: bool,
}

impl PushResampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { ratio: from as f64 / to as f64, pos: 1.0, prev: 0.0, bypass: from == to }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.bypass {
            out.extend_from_slice(input);
            return;
        }
        if input.is_empty() {
            return;
        }
        // Virtual buffer: index 0 = prev, index i = input[i-1]; length n = input.len() + 1.
        let n = input.len() + 1;
        let get = |i: usize| if i == 0 { self.prev } else { input[i - 1] };
        while (self.pos as usize) + 1 < n {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let a = get(i);
            let b = get(i + 1);
            out.push(a + (b - a) * frac);
            self.pos += self.ratio;
        }
        let idx = (self.pos as usize).min(n - 1);
        self.prev = get(idx);
        self.pos -= idx as f64;
    }
}

/// Pull-model linear resampler for playback: produces one output sample at a time, pulling source samples as needed.
pub struct PullResampler {
    step: f64, // source samples per output sample
    frac: f64,
    cur: f32,
    next: f32,
    bypass: bool,
}

impl PullResampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to as f64, frac: 0.0, cur: 0.0, next: 0.0, bypass: from == to }
    }

    #[inline]
    pub fn next(&mut self, mut src: impl FnMut() -> f32) -> f32 {
        if self.bypass {
            return src();
        }
        let out = self.cur + (self.next - self.cur) * self.frac as f32;
        self.frac += self.step;
        while self.frac >= 1.0 {
            self.frac -= 1.0;
            self.cur = self.next;
            self.next = src();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_resampler_keeps_rate() {
        let mut r = PushResampler::new(44100, 48000);
        let mut out = Vec::new();
        let input: Vec<f32> = (0..44100).map(|i| (i as f32 * 0.01).sin()).collect();
        for chunk in input.chunks(441) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 48000).abs() < 4, "got {}", out.len());
    }

    #[test]
    fn pull_resampler_consumes_rate() {
        let mut r = PullResampler::new(48000, 44100);
        let mut consumed = 0usize;
        for _ in 0..44100 {
            r.next(|| {
                consumed += 1;
                0.5
            });
        }
        assert!((consumed as i64 - 48000).abs() < 4, "consumed {consumed}");
    }
}

/// (microphone names, playback device names) for the settings window's dropdowns.
pub fn device_names() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let mics = host.input_devices().map(|it| it.filter_map(|d| d.name().ok()).collect()).unwrap_or_default();
    let outs = host.output_devices().map(|it| it.filter_map(|d| d.name().ok()).collect()).unwrap_or_default();
    (mics, outs)
}
