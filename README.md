# THIS IS ENTIRELY VIBECODED FOR MYSELF

# MicBridge

Use your laptop's microphone on the PC you stream from with [Sunshine](https://github.com/LizardByte/Sunshine)
(or [Apollo](https://github.com/ClassicOldSong/Apollo)) and [Moonlight](https://moonlight-stream.org/).

Moonlight and Sunshine don't carry microphone audio from the client back to the host. The forks that add it
have to patch both programs and break whenever either one updates. MicBridge takes a different route: it is a
small **sidecar** that runs next to Moonlight on the laptop and next to Sunshine on the streamed PC, carries
your mic over its own encrypted UDP stream, and plays it into a virtual microphone on the host. It never
touches Moonlight or Sunshine, so their updates can't break it.

```
 Laptop (client)                                 Streamed PC (host)
 ┌──────────────────────────┐                    ┌───────────────────────────────────────┐
 │ Moonlight.exe  (video in)│ ◄───── Sunshine ── │ Sunshine / Apollo                      │
 │                          │                    │                                        │
 │ micbridge client         │ ── Opus/AES-GCM ─► │ micbridge host ─► "CABLE Input"        │
 │   real microphone ─► Opus│     UDP 48020      │                     └► "CABLE Output" = │
 └──────────────────────────┘                    │        mic that games / Discord select │
                                                 └───────────────────────────────────────┘
```

## Features

* 48 kHz mono Opus, 20 ms frames, ~40 kbps, packet-loss concealment, adjustable jitter cushion (60 ms default).
* AES-256-GCM with a shared pairing key and replay protection. Wrong-key packets are dropped silently.
* Client only sends while `Moonlight.exe` is running, so both ends can live in the system tray permanently.
* Tray icon with status, mute, pairing-key copy; `--console` mode for troubleshooting.
* `micbridge firewall` and `micbridge autostart` helpers; everything else is one JSON file.
* Single exe, no installer. Windows 10/11 x64 on both ends.

## Downloads

Grab `micbridge.exe` from the [Releases](../../releases) page. Two builds are published; they share the same
config file and wire format and can talk to each other:

| build | exe size | notes |
|---|---|---|
| **Rust** (`micbridge-rust-win-x64.exe`) | ~1.5 MB | recommended |
| C# (`micbridge-csharp-win-x64.exe`) | ~66 MB | original implementation, self-contained .NET 8 |

## Quick start

On the **host** (the streamed PC): install [VB-Cable](https://vb-audio.com/Cable/) once, then

```
micbridge host --console      # prints the pairing key, Ctrl+C
micbridge firewall            # allow UDP 48020 (one admin prompt)
micbridge autostart           # start with Windows
```

and pick **CABLE Output** as the microphone in your game or voice app.

On the **laptop**:

```
micbridge client --host <host-ip> --key <pairing-key>
micbridge autostart
```

From then on the mic flows automatically whenever Moonlight is running. The full guide, settings table and
Sunshine prep-command alternative are in [rust/README.md](rust/README.md) (identical for the C# build).

## Repository layout

* [`rust/`](rust/) – Rust implementation (cpal, libopus, aes-gcm, tray-icon).
* [`csharp/`](csharp/) – C# implementation (.NET 8, NAudio, Concentus).

## Status

v0.1.0. Verified end to end on one machine (both builds, in every host/client combination). Not yet
battle-tested across two PCs over Wi-Fi; if you hear crackles raise `jitterMs` in `micbridge.json`.

## License

MIT, see [LICENSE](LICENSE). Opus is BSD-licensed; NAudio and Concentus are MIT.
