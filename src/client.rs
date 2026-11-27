//! Laptop side. Captures the microphone, encodes 20 ms Opus frames at 48 kHz mono and sends them to the host.
//! When follow_moonlight is on, it only captures and sends while a Moonlight process is running.

use crate::audio::{self, PushResampler};
use crate::config::Config;
use crate::protocol::{self, Opener, Sealer};
use crate::{log, platform};
use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FRAME: usize = 960; // 20 ms @ 48 kHz

pub struct Client {
    cfg: Config,
    socket: UdpSocket,
    target: SocketAddr,
    sealer: Mutex<Sealer>,
    stop: AtomicBool,
    muted: AtomicBool,
    active: AtomicBool,
    /// The mic has delivered nothing but exact zeros for a few seconds: almost always a denied mic permission.
    silent: AtomicBool,
    mic_name: Mutex<String>,
    last_error: Mutex<String>,
    last_pong: Mutex<Option<Instant>>,
    kbps: AtomicU32,
    sent_packets: AtomicU64,
    sent_bytes: AtomicU64,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// Samples captured by the cpal callback, already mono @ 48 kHz, waiting for the encoder thread.
struct CaptureQueue {
    q: Mutex<VecDeque<f32>>,
    cv: Condvar,
}

/// Everything that exists only while we're actively sending. Dropping it stops capture.
struct CaptureSession {
    _stream: cpal::Stream,
    running: Arc<AtomicBool>,
    queue: Arc<CaptureQueue>,
    encoder: Option<JoinHandle<()>>,
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.queue.cv.notify_all();
        if let Some(h) = self.encoder.take() {
            let _ = h.join();
        }
    }
}

impl Client {
    pub fn start(cfg: Config) -> Result<Arc<Client>> {
        let key = cfg.key_bytes()?;
        if cfg.host.trim().is_empty() {
            return Err(anyhow!("No host set. Run: sidemic client --host <host-ip> --key <key>"));
        }
        let target = resolve(&cfg.host, cfg.port)?;
        let bind: SocketAddr = if target.is_ipv6() { "[::]:0".parse().unwrap() } else { "0.0.0.0:0".parse().unwrap() };
        let socket = UdpSocket::bind(bind).context("opening UDP socket")?;
        socket.connect(target).context("connecting UDP socket")?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        log::info(format!(
            "Client mode. Host {target}, follow Moonlight = {}, bitrate {} bps",
            cfg.follow_moonlight, cfg.bitrate
        ));

        let client = Arc::new(Client {
            muted: AtomicBool::new(cfg.start_muted),
            cfg,
            socket,
            target,
            sealer: Mutex::new(Sealer::new(&key)),
            stop: AtomicBool::new(false),
            active: AtomicBool::new(false),
            silent: AtomicBool::new(false),
            mic_name: Mutex::new(String::new()),
            last_error: Mutex::new(String::new()),
            last_pong: Mutex::new(None),
            kbps: AtomicU32::new(0),
            sent_packets: AtomicU64::new(0),
            sent_bytes: AtomicU64::new(0),
            threads: Mutex::new(Vec::new()),
        });

        let rx = {
            let c = client.clone();
            let opener = Opener::new(&key);
            std::thread::Builder::new().name("pong-rx".into()).spawn(move || c.receive_loop(opener))?
        };
        let watch = {
            let c = client.clone();
            std::thread::Builder::new().name("moonlight-watch".into()).spawn(move || c.watch_loop())?
        };
        client.threads.lock().unwrap().extend([rx, watch]);
        Ok(client)
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
        log::info(if m { "Mic muted" } else { "Mic unmuted" });
    }

    pub fn connected(&self) -> bool {
        self.last_pong.lock().unwrap().map(|t| t.elapsed() < Duration::from_secs(3)).unwrap_or(false)
    }

    pub fn status(&self) -> String {
        let err = self.last_error.lock().unwrap().clone();
        if !err.is_empty() {
            return format!("Error: {err}");
        }
        if !self.active.load(Ordering::Relaxed) {
            return if self.cfg.follow_moonlight {
                format!("Idle - waiting for {}{}", self.cfg.moonlight_process, platform::EXE_SUFFIX)
            } else {
                "Idle".into()
            };
        }
        let link = if self.connected() { "connected" } else { "no reply from host yet" };
        let tail = if self.muted() {
            ", MUTED".to_string()
        } else if self.silent.load(Ordering::Relaxed) {
            format!(", mic is silent: {}", platform::MIC_PERMISSION_HINT)
        } else {
            format!(", {} kbps", self.kbps.load(Ordering::Relaxed))
        };
        format!("Sending {} to {} ({link}{tail})", self.mic_name.lock().unwrap(), self.target)
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        for h in self.threads.lock().unwrap().drain(..) {
            let _ = h.join();
        }
        log::info(format!(
            "Client stopped. Sent {} packets, {} KiB",
            self.sent_packets.load(Ordering::Relaxed),
            self.sent_bytes.load(Ordering::Relaxed) / 1024
        ));
    }

    fn watch_loop(self: Arc<Self>) {
        let mut session: Option<CaptureSession> = None;
        let mut seen = false; // last known answer from the process list
        let mut misses = 0u32; // consecutive polls without the process; stop only after two
        while !self.stop.load(Ordering::SeqCst) {
            if let Some(now) = platform::process_running(&self.cfg.moonlight_process) {
                seen = now;
            }
            misses = if seen { 0 } else { misses + 1 };
            let want = !self.cfg.follow_moonlight || seen || (session.is_some() && misses < 2);
            if want && session.is_none() {
                match self.start_capture() {
                    Ok(s) => {
                        session = Some(s);
                        self.active.store(true, Ordering::Relaxed);
                        self.last_error.lock().unwrap().clear();
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if *self.last_error.lock().unwrap() != msg {
                            log::error(format!("Capture: {msg}"));
                            *self.last_error.lock().unwrap() = msg;
                        }
                    }
                }
            } else if !want && session.is_some() {
                session = None;
                self.active.store(false, Ordering::Relaxed);
                log::info("Capture stopped");
            }
            std::thread::sleep(Duration::from_millis(1500));
        }
        if session.take().is_some() {
            self.active.store(false, Ordering::Relaxed);
            log::info("Capture stopped");
        }
    }

    fn start_capture(self: &Arc<Self>) -> Result<CaptureSession> {
        let device = audio::find_capture(&self.cfg.mic_device)?;
        let name = device.name().unwrap_or_else(|_| "microphone".into());
        let supported = device.default_input_config().context("querying microphone format")?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.clone().into();
        let channels = config.channels as usize;
        let rate = config.sample_rate.0;

        let queue = Arc::new(CaptureQueue { q: Mutex::new(VecDeque::with_capacity(48000)), cv: Condvar::new() });
        let running = Arc::new(AtomicBool::new(true));

        let stream = match sample_format {
            cpal::SampleFormat::F32 => build_input::<f32>(&device, &config, channels, rate, queue.clone())?,
            cpal::SampleFormat::I16 => build_input::<i16>(&device, &config, channels, rate, queue.clone())?,
            cpal::SampleFormat::U16 => build_input::<u16>(&device, &config, channels, rate, queue.clone())?,
            other => return Err(anyhow!("Unsupported microphone sample format {other:?}")),
        };
        stream.play().context("starting microphone")?;

        let encoder = {
            let c = self.clone();
            let q = queue.clone();
            let r = running.clone();
            std::thread::Builder::new()
                .name("mic-encoder".into())
                .spawn(move || c.encode_loop(q, r))?
        };

        *self.mic_name.lock().unwrap() = name.clone();
        log::info(format!("Capturing \"{name}\" ({rate} Hz, {channels} ch, {sample_format:?}) -> sending"));
        Ok(CaptureSession { _stream: stream, running, queue, encoder: Some(encoder) })
    }

    fn encode_loop(self: Arc<Self>, queue: Arc<CaptureQueue>, running: Arc<AtomicBool>) {
        let mut enc = match opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip) {
            Ok(e) => e,
            Err(e) => {
                *self.last_error.lock().unwrap() = format!("Opus encoder: {e}");
                return;
            }
        };
        let _ = enc.set_bitrate(opus::Bitrate::Bits(self.cfg.bitrate.clamp(8000, 128000)));
        let _ = enc.set_complexity(8);
        let _ = enc.set_inband_fec(true);
        let _ = enc.set_packet_loss_perc(10);
        let _ = enc.set_vbr(true);

        let mut frame = [0f32; FRAME];
        let mut opus_buf = [0u8; 1275];
        let mut last_ping = Instant::now() - Duration::from_secs(1);
        let threshold = FRAME + FRAME / 2; // 30 ms of audio queued before we encode
        // A real microphone always has some noise floor. Exact zeros for 3 s means the OS is feeding us silence,
        // which is what macOS (and Windows) do when the app has no microphone permission.
        let mut zero_frames = 0u32;
        self.silent.store(false, Ordering::Relaxed);

        while running.load(Ordering::SeqCst) {
            {
                let mut q = queue.q.lock().unwrap();
                while q.len() < threshold && running.load(Ordering::SeqCst) {
                    let (guard, _) = queue.cv.wait_timeout(q, Duration::from_millis(200)).unwrap();
                    q = guard;
                }
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                if q.len() > 48000 / 5 {
                    // 200 ms behind (startup burst or a stall). Stale audio only adds latency, so drop it.
                    q.clear();
                    drop(q);
                    log::info("Dropped capture backlog to keep latency low");
                    continue;
                }
                for s in frame.iter_mut() {
                    *s = q.pop_front().unwrap_or(0.0);
                }
            }
            if frame.iter().all(|&s| s == 0.0) {
                zero_frames = zero_frames.saturating_add(1);
                if zero_frames == 150 {
                    self.silent.store(true, Ordering::Relaxed);
                    log::warn(format!("Microphone delivers pure silence - {}", platform::MIC_PERMISSION_HINT));
                }
            } else if zero_frames > 0 {
                if zero_frames >= 150 {
                    log::info("Microphone signal is back");
                }
                zero_frames = 0;
                self.silent.store(false, Ordering::Relaxed);
            }
            if self.muted() {
                if last_ping.elapsed() >= Duration::from_millis(500) {
                    self.send(protocol::TYPE_PING, &[]);
                    last_ping = Instant::now();
                }
                continue;
            }
            match enc.encode_float(&frame, &mut opus_buf) {
                Ok(len) if len > 0 => self.send(protocol::TYPE_AUDIO, &opus_buf[..len]),
                Ok(_) => {}
                Err(e) => log::error(format!("Opus encode: {e}")),
            }
        }
    }

    fn send(&self, ptype: u8, payload: &[u8]) {
        let pkt = self.sealer.lock().unwrap().seal(ptype, payload);
        if let Err(e) = self.socket.send(&pkt) {
            if self.sent_packets.load(Ordering::Relaxed) == 0 {
                log::warn(format!("Send: {e}"));
            }
            return;
        }
        self.sent_packets.fetch_add(1, Ordering::Relaxed);
        self.sent_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
        RATE.add(pkt.len(), &self.kbps);
    }

    fn receive_loop(self: Arc<Self>, mut opener: Opener) {
        let mut buf = [0u8; protocol::MAX_PACKET];
        while !self.stop.load(Ordering::SeqCst) {
            let n = match self.socket.recv(&mut buf) {
                Ok(n) => n,
                // Timeouts, and ICMP "port unreachable" (ConnectionReset) while the host isn't up yet, are both normal.
                Err(_) => continue,
            };
            if let Some((hdr, _)) = opener.open(&buf[..n]) {
                if hdr.ptype == protocol::TYPE_PONG {
                    if !self.connected() {
                        log::info("Host replied - link is up");
                    }
                    *self.last_pong.lock().unwrap() = Some(Instant::now());
                }
            }
        }
    }
}

/// One-second bitrate meter shared by the sender path.
struct RateMeter {
    state: Mutex<(Instant, usize)>,
}

impl RateMeter {
    fn add(&self, bytes: usize, out: &AtomicU32) {
        let mut st = self.state.lock().unwrap();
        st.1 += bytes;
        let dt = st.0.elapsed().as_secs_f64();
        if dt >= 1.0 {
            out.store((st.1 as f64 * 8.0 / dt / 1000.0) as u32, Ordering::Relaxed);
            st.0 = Instant::now();
            st.1 = 0;
        }
    }
}

static RATE: std::sync::LazyLock<RateMeter> =
    std::sync::LazyLock::new(|| RateMeter { state: Mutex::new((Instant::now(), 0)) });

fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("Could not resolve host \"{host}\""))?
        .collect();
    addrs.sort_by_key(|a| a.is_ipv6()); // prefer IPv4
    addrs.into_iter().next().ok_or_else(|| anyhow!("Could not resolve host \"{host}\""))
}

fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    rate: u32,
    queue: Arc<CaptureQueue>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut resampler = PushResampler::new(rate, 48000);
    let mut mono: Vec<f32> = Vec::with_capacity(4096);
    let mut resampled: Vec<f32> = Vec::with_capacity(4096);
    let err_q = queue.clone();
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _| {
            mono.clear();
            resampled.clear();
            if channels <= 1 {
                mono.extend(data.iter().map(|s| f32::from_sample(*s)));
            } else {
                // Average the channels (same as the C# StereoToMono path).
                let inv = 1.0 / channels as f32;
                mono.extend(data.chunks_exact(channels).map(|fr| fr.iter().map(|s| f32::from_sample(*s)).sum::<f32>() * inv));
            }
            resampler.process(&mono, &mut resampled);
            let mut q = queue.q.lock().unwrap();
            q.extend(resampled.iter().copied());
            if q.len() > 48000 * 2 {
                let excess = q.len() - 48000 * 2;
                q.drain(..excess);
            }
            drop(q);
            queue.cv.notify_one();
        },
        move |e| {
            log::error(format!("Mic stream error: {e}"));
            err_q.cv.notify_all();
        },
        None,
    )?;
    Ok(stream)
}
