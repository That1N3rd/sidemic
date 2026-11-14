using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using Concentus;
using NAudio.CoreAudioApi;
using NAudio.Wave;
using NAudio.Wave.SampleProviders;

namespace MicBridge;

/// <summary>
/// Streamed-PC side. Listens for encrypted Opus frames, decodes them (with packet-loss concealment) and plays them
/// into the virtual microphone's playback endpoint (VB-Cable "CABLE Input" or "Speakers (Steam Streaming Microphone)").
/// Whatever is played there comes back out as a microphone that games, Discord, etc. can select.
/// </summary>
public sealed class MicHost : IDisposable
{
    private const int FrameSamples = 960;
    private readonly Config _cfg;
    private readonly byte[] _key;
    private readonly CancellationTokenSource _cts = new();

    private UdpClient? _udp;
    private PacketSealer? _sealer;
    private PacketOpener? _opener;
    private Thread? _rx;

    private MMDevice? _device;
    private WasapiOut? _out;
    private BufferedWaveProvider? _play;
    private IOpusDecoder? _dec;
    private readonly object _audioGate = new();

    // talker state
    private ulong _activeSession; private uint _expectedSeq; private bool _haveSession;
    private DateTime _lastPacket = DateTime.MinValue;
    private IPEndPoint? _talker;
    private readonly Dictionary<IPEndPoint, DateTime> _lastPong = new();
    private long _rxPackets, _lostPackets, _latePackets, _badPackets, _trimmed;
    private DateTime _rateMark = DateTime.UtcNow; private long _rateBytes; private volatile float _kbps;
    private volatile string _lastError = "";

    public MicHost(Config cfg) { _cfg = cfg; _key = cfg.KeyBytes(); }

    public string DeviceName => _device?.FriendlyName ?? "?";
    public bool Receiving => (DateTime.UtcNow - _lastPacket) < TimeSpan.FromSeconds(2);

    public string Status
    {
        get
        {
            if (_lastError.Length > 0) return "Error: " + _lastError;
            if (Receiving) return $"Receiving mic from {_talker?.Address} ({_kbps:0} kbps, loss {_lostPackets}) -> {DeviceName}";
            return $"Listening on UDP {_cfg.Port} -> {DeviceName}";
        }
    }

    public void Start()
    {
        _device = AudioDevices.FindVirtualMicOutput(_cfg.OutputDevice, out var hint)
                  ?? throw new InvalidOperationException(hint);

        // Build: 48 kHz mono 16-bit buffer -> float -> device channel count -> device sample rate -> WASAPI shared.
        var mix = _device.AudioClient.MixFormat;
        _play = new BufferedWaveProvider(new WaveFormat(48000, 16, 1))
        {
            BufferDuration = TimeSpan.FromSeconds(4), DiscardOnBufferOverflow = true, ReadFully = true,
        };
        ISampleProvider sp = _play.ToSampleProvider();
        if (mix.Channels == 2) sp = new MonoToStereoSampleProvider(sp);
        else if (mix.Channels > 2)
        {
            var mux = new MultiplexingSampleProvider(new[] { sp }, mix.Channels);
            for (var i = 0; i < mix.Channels; i++) mux.ConnectInputToOutput(0, i);
            sp = mux;
        }
        if (mix.SampleRate != 48000) sp = new WdlResamplingSampleProvider(sp, mix.SampleRate);

        _out = new WasapiOut(_device, AudioClientShareMode.Shared, true, 30);
        _out.PlaybackStopped += (_, e) =>
        {
            if (e.Exception != null) { _lastError = "Playback stopped: " + e.Exception.Message; Log.Error(_lastError); }
        };
        _out.Init(sp.ToWaveProvider());
        _out.Play();

        _dec = OpusCodecFactory.CreateDecoder(48000, 1);
        _sealer = new PacketSealer(_key);
        _opener = new PacketOpener(_key);

        try
        {
            _udp = new UdpClient(AddressFamily.InterNetworkV6);
            _udp.Client.DualMode = true;
            _udp.Client.Bind(new IPEndPoint(IPAddress.IPv6Any, _cfg.Port));
        }
        catch (SocketException)
        {
            _udp?.Dispose();
            _udp = new UdpClient(new IPEndPoint(IPAddress.Any, _cfg.Port));
        }
        Log.Info($"Host mode. Listening on UDP {_cfg.Port}; playing into \"{_device.FriendlyName}\" ({mix.SampleRate} Hz, {mix.Channels} ch); jitter cushion {_cfg.JitterMs} ms");

        _rx = new Thread(ReceiveLoop) { IsBackground = true, Name = "mic-rx", Priority = ThreadPriority.AboveNormal };
        _rx.Start();
    }

    private void ReceiveLoop()
    {
        var payload = new byte[Protocol.MaxPacket];
        var pcm = new short[FrameSamples * 6]; // room for up to 120 ms frames
        var pongBuf = new byte[Protocol.HeaderLen + Protocol.TagLen];

        while (!_cts.IsCancellationRequested)
        {
            byte[] data; IPEndPoint? remote = null;
            try { data = _udp!.Receive(ref remote); }
            catch (SocketException) { if (_cts.IsCancellationRequested) break; continue; }
            catch (ObjectDisposedException) { break; }

            var n = _opener!.Open(data, payload, out var type, out var session, out var seq);
            if (n < 0) { _badPackets++; if (_badPackets == 1 || _badPackets % 500 == 0) Log.Warn($"Dropped {_badPackets} packet(s) that failed authentication (wrong key or junk) from {remote}"); continue; }

            // Talker arbitration: one sender at a time; a new session takes over after 1 s of silence.
            var now = DateTime.UtcNow;
            if (!_haveSession || session != _activeSession)
            {
                if (_haveSession && (now - _lastPacket) < TimeSpan.FromSeconds(1)) continue; // someone else is talking
                _haveSession = true; _activeSession = session; _expectedSeq = seq; _talker = remote;
                lock (_audioGate) { _dec!.ResetState(); _play!.ClearBuffer(); }
                Log.Info($"New sender {remote} (session {session:x16})");
            }
            _lastPacket = now;
            MaybePong(remote!, pongBuf);

            if (type != Protocol.TypeAudio) continue;
            _rxPackets++; _rateBytes += data.Length;
            var dt = (now - _rateMark).TotalSeconds;
            if (dt >= 1) { _kbps = (float)(_rateBytes * 8 / dt / 1000); _rateBytes = 0; _rateMark = now; }

            if (seq < _expectedSeq && _expectedSeq - seq < 1000) { _latePackets++; continue; }
            var gap = seq - _expectedSeq;
            if (gap > 50) { Log.Warn($"Sequence jump of {gap}; resetting decoder"); lock (_audioGate) _dec!.ResetState(); gap = 0; }
            for (var i = 0; i < gap; i++) { _lostPackets++; Decode(ReadOnlySpan<byte>.Empty, pcm); }
            Decode(payload.AsSpan(0, n), pcm);
            _expectedSeq = seq + 1;
        }
    }

    private void Decode(ReadOnlySpan<byte> opus, short[] pcm)
    {
        int samples;
        lock (_audioGate)
        {
            try { samples = _dec!.Decode(opus, pcm, FrameSamples, false); }
            catch (Exception ex)
            {
                if (opus.Length > 0) Log.Warn("Opus decode: " + ex.Message);
                Array.Clear(pcm, 0, FrameSamples); samples = FrameSamples;
            }
            if (samples <= 0) return;

            var play = _play!;
            var cushion = TimeSpan.FromMilliseconds(Math.Clamp(_cfg.JitterMs, 20, 500));
            if (play.BufferedBytes == 0)
            {
                // Starting (or restarting after an underrun): lay down a silence cushion so network jitter doesn't cause gaps.
                var silence = new byte[play.WaveFormat.AverageBytesPerSecond * cushion.Ticks / TimeSpan.TicksPerSecond];
                play.AddSamples(silence, 0, silence.Length);
            }
            else if (play.BufferedDuration > cushion * 2 + TimeSpan.FromMilliseconds(40))
            {
                // Latency crept up (startup burst or client clock running fast). Drop this frame (20 ms) instead of
                // flushing everything; repeated a few times this walks the buffer back down with barely audible effect.
                _trimmed++;
                if (_trimmed == 1 || _trimmed % 100 == 0) Log.Info($"Trimmed {_trimmed} frame(s) to keep latency at ~{cushion.TotalMilliseconds} ms");
                return;
            }
            var bytes = MemoryMarshal.AsBytes(pcm.AsSpan(0, samples));
            play.AddSamples(bytes.ToArray(), 0, bytes.Length);
        }
    }

    private void MaybePong(IPEndPoint remote, byte[] buf)
    {
        var now = DateTime.UtcNow;
        if (_lastPong.TryGetValue(remote, out var t) && (now - t).TotalMilliseconds < 500) return;
        if (_lastPong.Count > 32) _lastPong.Clear();
        _lastPong[remote] = now;
        var len = _sealer!.Seal(Protocol.TypePong, ReadOnlySpan<byte>.Empty, buf);
        try { _udp!.Send(buf, len, remote); } catch (SocketException) { }
    }

    public void Dispose()
    {
        _cts.Cancel();
        try { _udp?.Close(); } catch { }
        _rx?.Join(500);
        try { _out?.Stop(); } catch { }
        _out?.Dispose();
        _sealer?.Dispose(); _opener?.Dispose();
        Log.Info($"Host stopped. Received {_rxPackets} packets, lost {_lostPackets}, late {_latePackets}, trimmed {_trimmed}, rejected {_badPackets}");
    }
}
