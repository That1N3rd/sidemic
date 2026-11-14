using System.Buffers.Binary;
using System.Security.Cryptography;

namespace MicBridge;

/// <summary>
/// Wire format (UDP), 16-byte header then AES-256-GCM ciphertext + 16-byte tag:
///   0..1   "MB"
///   2      version (1)
///   3      type: 1 = audio (one Opus 20 ms frame), 2 = ping (client keepalive), 3 = pong (host reply)
///   4..11  session id (random per process start, chosen by the sender)
///   12..15 sequence number (big endian, per sender)
/// Nonce = session id || sequence (12 bytes). Each sender has its own session id so nonces never collide across directions.
/// The header is authenticated as additional data, so a packet with the wrong key or a tampered header is dropped silently.
/// </summary>
public static class Protocol
{
    public const int HeaderLen = 16;
    public const int TagLen = 16;
    public const byte Version = 1;
    public const byte TypeAudio = 1;
    public const byte TypePing = 2;
    public const byte TypePong = 3;
    public const int MaxPacket = 1500;

    public static void WriteHeader(Span<byte> dst, byte type, ReadOnlySpan<byte> session, uint seq)
    {
        dst[0] = (byte)'M'; dst[1] = (byte)'B'; dst[2] = Version; dst[3] = type;
        session.CopyTo(dst.Slice(4, 8));
        BinaryPrimitives.WriteUInt32BigEndian(dst.Slice(12, 4), seq);
    }

    public static bool ParseHeader(ReadOnlySpan<byte> pkt, out byte type, out ulong session, out uint seq)
    {
        type = 0; session = 0; seq = 0;
        if (pkt.Length < HeaderLen + TagLen) return false;
        if (pkt[0] != 'M' || pkt[1] != 'B' || pkt[2] != Version) return false;
        type = pkt[3];
        session = BinaryPrimitives.ReadUInt64BigEndian(pkt.Slice(4, 8));
        seq = BinaryPrimitives.ReadUInt32BigEndian(pkt.Slice(12, 4));
        return true;
    }
}

/// <summary>Encrypts outgoing packets for one sender (one session id + counter).</summary>
public sealed class PacketSealer : IDisposable
{
    private readonly AesGcm _aes;
    private readonly byte[] _session = RandomNumberGenerator.GetBytes(8);
    private uint _seq;

    public PacketSealer(byte[] key) => _aes = new AesGcm(key, Protocol.TagLen);

    public ulong SessionId => BinaryPrimitives.ReadUInt64BigEndian(_session);

    /// <summary>Builds a full packet into <paramref name="dst"/>; returns its length.</summary>
    public int Seal(byte type, ReadOnlySpan<byte> payload, Span<byte> dst)
    {
        var seq = _seq++;
        Protocol.WriteHeader(dst, type, _session, seq);
        Span<byte> nonce = stackalloc byte[12];
        _session.CopyTo(nonce);
        BinaryPrimitives.WriteUInt32BigEndian(nonce.Slice(8), seq);
        var ct = dst.Slice(Protocol.HeaderLen, payload.Length);
        var tag = dst.Slice(Protocol.HeaderLen + payload.Length, Protocol.TagLen);
        _aes.Encrypt(nonce, payload, ct, tag, dst.Slice(0, Protocol.HeaderLen));
        return Protocol.HeaderLen + payload.Length + Protocol.TagLen;
    }

    public void Dispose() => _aes.Dispose();
}

/// <summary>Authenticates and decrypts incoming packets, with a sliding anti-replay window per sender session.</summary>
public sealed class PacketOpener : IDisposable
{
    private readonly AesGcm _aes;
    private readonly Dictionary<ulong, ReplayWindow> _windows = new();

    public PacketOpener(byte[] key) => _aes = new AesGcm(key, Protocol.TagLen);

    /// <summary>Returns payload length written to <paramref name="payload"/>, or -1 if the packet is invalid or replayed.</summary>
    public int Open(ReadOnlySpan<byte> pkt, Span<byte> payload, out byte type, out ulong session, out uint seq)
    {
        if (!Protocol.ParseHeader(pkt, out type, out session, out seq)) return -1;
        var ctLen = pkt.Length - Protocol.HeaderLen - Protocol.TagLen;
        if (ctLen < 0 || ctLen > payload.Length) return -1;

        Span<byte> nonce = stackalloc byte[12];
        pkt.Slice(4, 8).CopyTo(nonce);
        BinaryPrimitives.WriteUInt32BigEndian(nonce.Slice(8), seq);
        try
        {
            _aes.Decrypt(nonce, pkt.Slice(Protocol.HeaderLen, ctLen), pkt.Slice(Protocol.HeaderLen + ctLen, Protocol.TagLen),
                payload.Slice(0, ctLen), pkt.Slice(0, Protocol.HeaderLen));
        }
        catch (CryptographicException) { return -1; }

        // Only authenticated packets touch replay state, so garbage can't pollute it.
        if (!_windows.TryGetValue(session, out var win))
        {
            if (_windows.Count > 64) _windows.Clear();
            win = new ReplayWindow();
            _windows[session] = win;
        }
        return win.Accept(seq) ? ctLen : -1;
    }

    public void Dispose() => _aes.Dispose();

    /// <summary>128-packet sliding bitmap, like IPsec/DTLS replay protection.</summary>
    private sealed class ReplayWindow
    {
        private uint _max; private bool _any;
        private ulong _lo, _hi; // bit i set => seq (_max - i) seen; i in 0..127

        public bool Accept(uint seq)
        {
            if (!_any) { _any = true; _max = seq; _lo = 1; return true; }
            if (seq > _max)
            {
                var shift = seq - _max;
                if (shift >= 128) { _lo = 1; _hi = 0; }
                else
                {
                    for (var i = 0; i < shift; i++) { _hi = (_hi << 1) | (_lo >> 63); _lo <<= 1; }
                    _lo |= 1;
                }
                _max = seq; return true;
            }
            var back = _max - seq;
            if (back >= 128) return false;
            if (back < 64) { var m = 1UL << (int)back; if ((_lo & m) != 0) return false; _lo |= m; return true; }
            else { var m = 1UL << (int)(back - 64); if ((_hi & m) != 0) return false; _hi |= m; return true; }
        }
    }
}
