using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using Concentus;
using Concentus.Enums;
using NAudio.CoreAudioApi;
using NAudio.Wave;
using NAudio.Wave.SampleProviders;

namespace MicBridge;

/// <summary>
/// Laptop side. Captures the microphone, encodes 20 ms Opus frames at 48 kHz mono and sends them to the host.
/// When FollowMoonlight is on, it only captures and sends while a Moonlight process is running.
/// </summary>
public sealed class MicClient : IDisposable
{
    private const int FrameSamples = 960; // 20 ms @ 48 kHz
    private readonly Config _cfg;
    private readonly byte[] _key;
    private readonly CancellationTokenSource _cts = new();

    private UdpClient? _udp;
    private PacketSealer? _sealer;
    private PacketOpener? _opener;
    private Thread? _watcher, _receiver;

    // capture pipeline (alive only while Active)
    private WasapiCapture? _capture;
    private BufferedWaveProvider? _buf;
    private ISampleProvider? _chain;
    private Thread? _encoderThread;
    private readonly AutoResetEvent _dataSignal = new(false);
    private volatile bool _captureRunning;

    private volatile bool _muted;
    private volatile bool _active;
    private volatile string _micName = "";
    private volatile string _lastError = "";
    private DateTime _lastPong = DateTime.MinValue;
    private long _sentPackets, _sentBytes;
    private DateTime _rateMark = DateTime.UtcNow; private long _rateBytes; private volatile float _kbps;

    public MicClient(Config cfg)
    {
        _cfg = cfg;
        _key = cfg.KeyBytes();
        _muted = cfg.StartMuted;
    }

    public bool Muted { get => _muted; set { _muted = value; Log.Info(value ? "Mic muted" : "Mic unmuted"); } }
    public bool Connected => (DateTime.UtcNow - _lastPong) < TimeSpan.FromSeconds(3);

    public string Status
    {
        get
        {
            if (_lastError.Length > 0) return "Error: " + _lastError;
            if (!_active) return _cfg.FollowMoonlight ? $"Idle - waiting for {_cfg.MoonlightProcess}.exe" : "Idle";
            var link = Connected ? "connected" : "no reply from host yet";
            var mute = _muted ? ", MUTED" : $", {_kbps:0} kbps";
            return $"Sending {_micName} to {_cfg.Host}:{_cfg.Port} ({link}{mute})";
        }
    }

    public void Start()
    {
        var addr = Resolve(_cfg.Host);
        _udp = new UdpClient(addr.AddressFamily);
        _udp.Connect(new IPEndPoint(addr, _cfg.Port));
        _sealer = new PacketSealer(_key);
        _opener = new PacketOpener(_key);
        Log.Info($"Client mode. Host {addr}:{_cfg.Port}, follow Moonlight = {_cfg.FollowMoonlight}, bitrate {_cfg.Bitrate} bps");

        _receiver = new Thread(ReceiveLoop) { IsBackground = true, Name = "pong-rx" };
        _receiver.Start();
        _watcher = new Thread(WatchLoop) { IsBackground = true, Name = "moonlight-watch" };
        _watcher.Start();
    }

    private static IPAddress Resolve(string host)
    {
        if (string.IsNullOrWhiteSpace(host)) throw new InvalidOperationException("No host set. Run: micbridge client --host <host-ip> --key <key>");
        if (IPAddress.TryParse(host, out var ip)) return ip;
        var all = Dns.GetHostAddresses(host);
        return all.FirstOrDefault(a => a.AddressFamily == AddressFamily.InterNetwork) ?? all.FirstOrDefault()
               ?? throw new InvalidOperationException($"Could not resolve host \"{host}\"");
    }

    private void WatchLoop()
    {
        while (!_cts.IsCancellationRequested)
        {
            var want = !_cfg.FollowMoonlight || MoonlightRunning();
            try
            {
                if (want && !_active) StartCapture();
                else if (!want && _active) StopCapture();
            }
            catch (Exception ex)
            {
                _lastError = ex.Message;
                Log.Error("Capture: " + ex.Message);
                StopCapture();
            }
            _cts.Token.WaitHandle.WaitOne(1500);
        }
        StopCapture();
    }

    private bool MoonlightRunning()
    {
        var procs = Process.GetProcessesByName(_cfg.MoonlightProcess);
        var any = procs.Length > 0;
        foreach (var p in procs) p.Dispose();
        return any;
    }

    private void StartCapture()
    {
        var dev = AudioDevices.FindCapture(_cfg.MicDevice)
                  ?? throw new InvalidOperationException(string.IsNullOrEmpty(_cfg.MicDevice)
                      ? "No default microphone. Plug one in or set micDevice in micbridge.json."
                      : $"No microphone matches \"{_cfg.MicDevice}\". Run: micbridge devices");
        _micName = dev.FriendlyName;

        _capture = new WasapiCapture(dev, true, 20);
        var fmt = _capture.WaveFormat;
        _buf = new BufferedWaveProvider(fmt) { BufferDuration = TimeSpan.FromSeconds(2), DiscardOnBufferOverflow = true, ReadFully = true };

        ISampleProvider sp = _buf.ToSampleProvider();
        if (fmt.Channels == 2) sp = new StereoToMonoSampleProvider(sp) { LeftVolume = 0.5f, RightVolume = 0.5f };
        else if (fmt.Channels > 2) sp = new MultiplexingSampleProvider(new[] { sp }, 1); // channel 0 only
        if (fmt.SampleRate != 48000) sp = new WdlResamplingSampleProvider(sp, 48000);
        _chain = sp;

        _capture.DataAvailable += (_, e) => { _buf.AddSamples(e.Buffer, 0, e.BytesRecorded); _dataSignal.Set(); };
        _capture.RecordingStopped += (_, e) =>
        {
            if (e.Exception != null) { _lastError = "Mic stopped: " + e.Exception.Message; Log.Error(_lastError); }
        };

        _captureRunning = true;
        _encoderThread = new Thread(EncodeLoop) { IsBackground = true, Name = "mic-encoder", Priority = ThreadPriority.AboveNormal };
        _encoderThread.Start();
        _capture.StartRecording();
        _active = true;
        _lastError = "";
        Log.Info($"Capturing \"{_micName}\" ({fmt.SampleRate} Hz, {fmt.Channels} ch, {fmt.Encoding}) -> sending");
    }

    private void StopCapture()
    {
        if (!_active && _capture == null) return;
        _active = false;
        _captureRunning = false;
        _dataSignal.Set();
        try { _capture?.StopRecording(); } catch { }
        _encoderThread?.Join(1000);
        _capture?.Dispose();
        _capture = null; _buf = null; _chain = null; _encoderThread = null;
        Log.Info("Capture stopped");
    }

    private void EncodeLoop()
    {
        var enc = OpusCodecFactory.CreateEncoder(48000, 1, OpusApplication.OPUS_APPLICATION_VOIP);
        enc.Bitrate = Math.Clamp(_cfg.Bitrate, 8000, 128000);
        enc.Complexity = 8;
        enc.SignalType = OpusSignal.OPUS_SIGNAL_VOICE;
        enc.UseInbandFEC = true;
        enc.PacketLossPercent = 10;
        enc.UseVBR = true;

        var frame = new float[FrameSamples];
        var opus = new byte[1275];
        var pkt = new byte[Protocol.MaxPacket];
        var lastPing = DateTime.MinValue;
        var chain = _chain!; var buf = _buf!;
        var threshold = TimeSpan.FromMilliseconds(30);

        while (_captureRunning)
        {
            _dataSignal.WaitOne(200);
            if (buf.BufferedDuration > TimeSpan.FromMilliseconds(200))
            {
                // We fell behind (startup burst, or the machine stalled). Stale audio only adds latency, so drop it.
                buf.ClearBuffer();
                Log.Info("Dropped capture backlog to keep latency low");
                continue;
            }
            while (_captureRunning && buf.BufferedDuration >= threshold)
            {
                var n = chain.Read(frame, 0, FrameSamples);
                if (n < FrameSamples) break;
                if (_muted)
                {
                    if ((DateTime.UtcNow - lastPing).TotalMilliseconds > 500) { Send(Protocol.TypePing, ReadOnlySpan<byte>.Empty, pkt); lastPing = DateTime.UtcNow; }
                    continue;
                }
                int len;
                try { len = enc.Encode(frame, FrameSamples, opus, opus.Length); }
                catch (Exception ex) { Log.Error("Opus encode: " + ex.Message); continue; }
                if (len > 0) Send(Protocol.TypeAudio, opus.AsSpan(0, len), pkt);
            }
        }
    }

    private void Send(byte type, ReadOnlySpan<byte> payload, byte[] pkt)
    {
        var sealer = _sealer; var udp = _udp;
        if (sealer == null || udp == null) return;
        int len;
        lock (sealer) len = sealer.Seal(type, payload, pkt);
        try { udp.Send(pkt, len); }
        catch (SocketException ex) { if (_lastError.Length == 0) Log.Warn("Send: " + ex.Message); return; }
        _sentPackets++; _sentBytes += len; _rateBytes += len;
        var dt = (DateTime.UtcNow - _rateMark).TotalSeconds;
        if (dt >= 1) { _kbps = (float)(_rateBytes * 8 / dt / 1000); _rateBytes = 0; _rateMark = DateTime.UtcNow; }
    }

    private void ReceiveLoop()
    {
        var payload = new byte[Protocol.MaxPacket];
        while (!_cts.IsCancellationRequested)
        {
            byte[] data;
            try { IPEndPoint? ep = null; data = _udp!.Receive(ref ep); }
            catch (SocketException ex)
            {
                // ICMP port unreachable shows up here when the host isn't listening; keep going.
                if (ex.SocketErrorCode == SocketError.ConnectionReset) { Thread.Sleep(200); continue; }
                if (_cts.IsCancellationRequested) break;
                Thread.Sleep(200); continue;
            }
            catch (ObjectDisposedException) { break; }
            var n = _opener!.Open(data, payload, out var type, out _, out _);
            if (n >= 0 && type == Protocol.TypePong)
            {
                if (!Connected) Log.Info("Host replied - link is up");
                _lastPong = DateTime.UtcNow;
            }
        }
    }

    public void Dispose()
    {
        _cts.Cancel();
        StopCapture();
        try { _udp?.Close(); } catch { }
        _watcher?.Join(500); _receiver?.Join(500);
        _sealer?.Dispose(); _opener?.Dispose();
        Log.Info($"Client stopped. Sent {_sentPackets} packets, {_sentBytes / 1024} KiB");
    }
}
