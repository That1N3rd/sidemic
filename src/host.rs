//! Streamed-PC side. Listens for encrypted Opus frames, decodes them (with packet-loss concealment) and plays
//! them into the virtual microphone's playback endpoint (VB-Cable "CABLE Input" or "Speakers (Steam Streaming
//! Microphone)"). Whatever is played there comes back out as a microphone that games, Discord, etc. can select.

use crate::audio::{self, PullResampler};
use crate::config::Config;
use crate::log;
use crate::protocol::{self, Opener, Sealer};
use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FRAME: usize = 960;

/// Decoded 48 kHz mono audio waiting for the output callback, plus the jitter-cushion logic.
struct PlayBuf {
    q: VecDeque<i16>,
    cushion: usize, // samples of silence laid down whenever we start from empty
    trimmed: u64,
}

impl PlayBuf {
    fn push_frame(&mut self, pcm: &[i16]) {
        if self.q.is_empty() {
            // Starting (or restarting after an underrun): lay down a silence cushion so jitter doesn't cause gaps.
            self.q.extend(std::iter::repeat_n(0i16, self.cushion));
        } else if self.q.len() > self.cushion * 2 + 48 * 40 {
            // Latency crept up (startup burst or client clock running fast). Drop this 20 ms frame instead of
            // flushing everything; repeated a few times this walks the buffer back down with barely audible effect.
            self.trimmed += 1;
            if self.trimmed == 1 || self.trimmed % 100 == 0 {
                log::info(format!("Trimmed {} frame(s) to keep latency at ~{} ms", self.trimmed, self.cushion / 48));
            }
            return;
        }
        self.q.extend(pcm.iter().copied());
    }
}

pub struct Host {
    cfg: Config,
    socket: UdpSocket,
    sealer: Mutex<Sealer>,
    play: Arc<Mutex<PlayBuf>>,
    stop: AtomicBool,
    device_name: String,
    last_error: Mutex<String>,
    last_packet: Mutex<Option<Instant>>,
    talker: Mutex<Option<SocketAddr>>,
    kbps: AtomicU32,
    rx_packets: AtomicU64,
    lost: AtomicU64,
    late: AtomicU64,
    bad: AtomicU64,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Host {
    pub fn start(cfg: Config) -> Result<Arc<Host>> {
        let key = cfg.key_bytes()?;
        let device = audio::find_virtual_mic_output(&cfg.output_device)?;
        let device_name = device.name().unwrap_or_else(|_| "output".into());
        let supported = device.default_output_config().context("querying playback format")?;
        let out_rate = supported.sample_rate().0;
        let out_channels = supported.channels();

        let cushion = (cfg.jitter_ms.clamp(20, 500) as usize) * 48;
        let play = Arc::new(Mutex::new(PlayBuf { q: VecDeque::with_capacity(48000), cushion, trimmed: 0 }));

        let socket = UdpSocket::bind(("0.0.0.0", cfg.port)).with_context(|| format!("binding UDP port {}", cfg.port))?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        log::info(format!(
            "Host mode. Listening on UDP {}; playing into \"{device_name}\" ({out_rate} Hz, {out_channels} ch); jitter cushion {} ms",
            cfg.port, cfg.jitter_ms
        ));

        let host = Arc::new(Host {
            cfg,
            socket,
            sealer: Mutex::new(Sealer::new(&key)),
            play,
            stop: AtomicBool::new(false),
            device_name,
            last_error: Mutex::new(String::new()),
            last_packet: Mutex::new(None),
            talker: Mutex::new(None),
            kbps: AtomicU32::new(0),
            rx_packets: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            late: AtomicU64::new(0),
            bad: AtomicU64::new(0),
            threads: Mutex::new(Vec::new()),
        });

        // The cpal stream lives on its own thread for its whole life (cpal streams aren't Send on every platform).
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let audio_thread = {
            let h = host.clone();
            std::thread::Builder::new().name("mic-playback".into()).spawn(move || {
                let stream = build_output(&device, &supported, h.play.clone());
                match stream {
                    Ok(s) => {
                        if let Err(e) = s.play() {
                            let _ = ready_tx.send(Err(anyhow!("starting playback: {e}")));
                            return;
                        }
                        let _ = ready_tx.send(Ok(()));
                        while !h.stop.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        drop(s);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?
        };
        ready_rx.recv().map_err(|_| anyhow!("playback thread died"))??;

        let rx_thread = {
            let h = host.clone();
            let opener = Opener::new(&key);
            std::thread::Builder::new().name("mic-rx".into()).spawn(move || h.receive_loop(opener))?
        };
        host.threads.lock().unwrap().extend([audio_thread, rx_thread]);
        Ok(host)
    }

    pub fn receiving(&self) -> bool {
        self.last_packet.lock().unwrap().map(|t| t.elapsed() < Duration::from_secs(2)).unwrap_or(false)
    }

    pub fn status(&self) -> String {
        let err = self.last_error.lock().unwrap().clone();
        if !err.is_empty() {
            return format!("Error: {err}");
        }
        if self.receiving() {
            let who = self.talker.lock().unwrap().map(|a| a.ip().to_string()).unwrap_or_default();
            return format!(
                "Receiving mic from {who} ({} kbps, loss {}) -> {}",
                self.kbps.load(Ordering::Relaxed),
                self.lost.load(Ordering::Relaxed),
                self.device_name
            );
        }
        format!("Listening on UDP {} -> {}", self.cfg.port, self.device_name)
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        for h in self.threads.lock().unwrap().drain(..) {
            let _ = h.join();
        }
        log::info(format!(
            "Host stopped. Received {} packets, lost {}, late {}, trimmed {}, rejected {}",
            self.rx_packets.load(Ordering::Relaxed),
            self.lost.load(Ordering::Relaxed),
            self.late.load(Ordering::Relaxed),
            self.play.lock().unwrap().trimmed,
            self.bad.load(Ordering::Relaxed)
        ));
    }

    fn receive_loop(self: Arc<Self>, mut opener: Opener) {
        let mut dec = match opus::Decoder::new(48000, opus::Channels::Mono) {
            Ok(d) => d,
            Err(e) => {
                *self.last_error.lock().unwrap() = format!("Opus decoder: {e}");
                return;
            }
        };
        let mut buf = [0u8; protocol::MAX_PACKET];
        let mut pcm = vec![0i16; FRAME * 6];
        let mut active: Option<(u64, u32)> = None; // (session, expected seq)
        let mut last_pong: HashMap<SocketAddr, Instant> = HashMap::new();
        let mut rate = (Instant::now(), 0usize);

        while !self.stop.load(Ordering::SeqCst) {
            let (n, remote) = match self.socket.recv_from(&mut buf) {
                Ok(x) => x,
                Err(_) => continue,
            };
            let Some((hdr, payload)) = opener.open(&buf[..n]) else {
                let bad = self.bad.fetch_add(1, Ordering::Relaxed) + 1;
                if bad == 1 || bad % 500 == 0 {
                    log::warn(format!("Dropped {bad} packet(s) that failed authentication (wrong key or junk) from {remote}"));
                }
                continue;
            };

            // Talker arbitration: one sender at a time; a new session takes over after 1 s of silence.
            let now = Instant::now();
            let fresh = self.last_packet.lock().unwrap().map(|t| now - t < Duration::from_secs(1)).unwrap_or(false);
            match active {
                Some((s, _)) if s == hdr.session => {}
                Some(_) if fresh => continue, // someone else is talking
                _ => {
                    active = Some((hdr.session, hdr.seq));
                    *self.talker.lock().unwrap() = Some(remote);
                    let _ = dec.reset_state();
                    self.play.lock().unwrap().q.clear();
                    log::info(format!("New sender {remote} (session {:016x})", hdr.session));
                }
            }
            *self.last_packet.lock().unwrap() = Some(now);
            self.maybe_pong(remote, &mut last_pong);

            if hdr.ptype != protocol::TYPE_AUDIO {
                continue;
            }
            self.rx_packets.fetch_add(1, Ordering::Relaxed);
            rate.1 += n;
            let dt = rate.0.elapsed().as_secs_f64();
            if dt >= 1.0 {
                self.kbps.store((rate.1 as f64 * 8.0 / dt / 1000.0) as u32, Ordering::Relaxed);
                rate = (now, 0);
            }

            let expected = active.as_ref().unwrap().1;
            if hdr.seq < expected && expected - hdr.seq < 1000 {
                self.late.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let mut gap = hdr.seq.wrapping_sub(expected);
            if gap > 50 {
                log::warn(format!("Sequence jump of {gap}; resetting decoder"));
                let _ = dec.reset_state();
                gap = 0;
            }
            for _ in 0..gap {
                self.lost.fetch_add(1, Ordering::Relaxed);
                self.decode(&mut dec, &[], &mut pcm);
            }
            self.decode(&mut dec, &payload, &mut pcm);
            active = Some((hdr.session, hdr.seq.wrapping_add(1)));
        }
    }

    fn decode(&self, dec: &mut opus::Decoder, data: &[u8], pcm: &mut [i16]) {
        let samples = match dec.decode(data, &mut pcm[..FRAME], false) {
            Ok(n) => n,
            Err(e) => {
                if !data.is_empty() {
                    log::warn(format!("Opus decode: {e}"));
                }
                pcm[..FRAME].fill(0);
                FRAME
            }
        };
        if samples > 0 {
            self.play.lock().unwrap().push_frame(&pcm[..samples]);
        }
    }

    fn maybe_pong(&self, remote: SocketAddr, last: &mut HashMap<SocketAddr, Instant>) {
        let now = Instant::now();
        if last.get(&remote).map(|t| now - *t < Duration::from_millis(500)).unwrap_or(false) {
            return;
        }
        if last.len() > 32 {
            last.clear();
        }
        last.insert(remote, now);
        let pkt = self.sealer.lock().unwrap().seal(protocol::TYPE_PONG, &[]);
        let _ = self.socket.send_to(&pkt, remote);
    }
}

fn build_output(device: &cpal::Device, supported: &cpal::SupportedStreamConfig, play: Arc<Mutex<PlayBuf>>) -> Result<cpal::Stream> {
    let fmt = supported.sample_format();
    let config: cpal::StreamConfig = supported.clone().into();
    match fmt {
        cpal::SampleFormat::F32 => build_output_typed::<f32>(device, &config, play),
        cpal::SampleFormat::I16 => build_output_typed::<i16>(device, &config, play),
        cpal::SampleFormat::U16 => build_output_typed::<u16>(device, &config, play),
        other => Err(anyhow!("Unsupported playback sample format {other:?}")),
    }
}

fn build_output_typed<T>(device: &cpal::Device, config: &cpal::StreamConfig, play: Arc<Mutex<PlayBuf>>) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut resampler = PullResampler::new(48000, config.sample_rate.0);
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            let mut pb = play.lock().unwrap();
            for frame in data.chunks_exact_mut(channels) {
                let s = resampler.next(|| pb.q.pop_front().map(|v| v as f32 / 32768.0).unwrap_or(0.0));
                let v = T::from_sample(s);
                for ch in frame.iter_mut() {
                    *ch = v;
                }
            }
        },
        |e| log::error(format!("Playback stream error: {e}")),
        None,
    )?;
    Ok(stream)
}
