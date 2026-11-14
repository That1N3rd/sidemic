# MicBridge (Rust)

Use your laptop's microphone on the PC you stream from with Sunshine (or Apollo) and Moonlight.

MicBridge is a tiny sidecar, not a fork. It runs next to Moonlight on the laptop and next to Sunshine on
the streamed PC, carries your mic over its own encrypted UDP stream, and plays it into a virtual microphone
on the host. Because it never touches Moonlight or Sunshine, updating either of them cannot break it.

This is the Rust port of the original C# version in `../micbridge`. Same features, same config file, same
wire format (the two builds interoperate), but a ~2 MB exe instead of 66 MB and far less memory.

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

* 48 kHz mono Opus, 20 ms frames, ~40 kbps, packet-loss concealment, ~60 ms jitter cushion.
* AES-256-GCM with a shared key and replay protection. Packets with the wrong key are dropped before anything else happens.
* Client only sends while `Moonlight.exe` is running (configurable), so it can sit in the tray forever.
* Single `micbridge.exe` for Windows 10/11 x64. No installer, no runtime to install.

## Setup

### 1. Host (the streamed PC)

1. Install a virtual microphone, once. Either:
   * **VB-Cable** (recommended): https://vb-audio.com/Cable/ . Run the installer as administrator, reboot if asked.
     This gives you a playback device `CABLE Input` and a microphone `CABLE Output`.
   * **or** the **Steam Streaming Microphone** that Sunshine's optional audio driver installs. Open `mmsys.cpl`,
     right-click > *Show Disabled Devices*, and enable *Speakers (Steam Streaming Microphone)* on the Playback tab
     and *Microphone (Steam Streaming Microphone)* on the Recording tab.
2. Copy `micbridge.exe` somewhere permanent (for example `C:\Tools\MicBridge\`).
3. Open a terminal in that folder and run:
   ```
   micbridge host --console
   ```
   It prints the **pairing key** and starts listening on UDP 48020. Press Ctrl+C.
   (`micbridge host` without `--console` does the same from the system tray; the key is in the tray menu > *Copy pairing key*.)
4. Allow the port through Windows Firewall (asks for admin once):
   ```
   micbridge firewall
   ```
5. Make it start with Windows:
   ```
   micbridge autostart
   ```
6. In the game / Discord / whatever on the host, pick **CABLE Output** (or *Microphone (Steam Streaming Microphone)*)
   as the microphone. Optionally set it as the default *communications* device in `mmsys.cpl` > Recording.

### 2. Client (the laptop)

1. Copy `micbridge.exe` somewhere permanent.
2. Run once with the host's address and the pairing key:
   ```
   micbridge client --host 192.168.1.50 --key PASTE_KEY_HERE
   ```
   Use the same IP you added in Moonlight. A hostname works too.
3. Make it start with Windows:
   ```
   micbridge autostart
   ```

That's it. From now on it sits in the tray; when Moonlight starts, the mic starts flowing, and the tray
tooltip shows *connected* once the host answers. Right-click the tray icon to mute.

## Checking it works

* Host tray tooltip changes from *Listening on UDP 48020* to *Receiving mic from <laptop ip> (… kbps)*.
* On the host, `mmsys.cpl` > Recording > `CABLE Output` shows a moving level meter while you talk.
* Both ends write `micbridge.log` next to the exe (or in `%LOCALAPPDATA%\MicBridge` if that folder is read-only).

## Settings

Everything lives in `micbridge.json` next to the exe (tray menu > *Open config file*). Command-line flags
overwrite it, so you can also re-run `micbridge client --host … --key …` to change things.

| key | default | meaning |
|---|---|---|
| `mode` | | `host` or `client` |
| `host` | | client: host PC address |
| `port` | `48020` | UDP port, same on both ends |
| `key` | | shared key (base64, 32 bytes). Host generates it on first run |
| `micDevice` | `""` | client: part of the mic's name; empty = Windows default mic (`micbridge devices` lists names) |
| `outputDevice` | `""` | host: part of the playback device feeding the virtual mic; empty = auto (`CABLE Input`, then `Steam Streaming Microphone`) |
| `bitrate` | `40000` | Opus bits/s. 24000 is fine for voice, 64000 is very clean |
| `followMoonlight` | `true` | client: only send while `moonlightProcess` runs |
| `moonlightProcess` | `Moonlight` | client: process name without `.exe` |
| `jitterMs` | `60` | host: cushion against network jitter. Raise to 100-150 on Wi-Fi/Internet if you hear crackles |
| `startMuted` | `false` | client: start muted |

## Running it from Sunshine instead of at startup

If you'd rather the host side only run while a stream is active, add it as a global prep command in
Sunshine's web UI (Configuration > General > Command Preparations), or per application:

* Do: `"C:\Tools\MicBridge\micbridge.exe" host`
* Undo: `taskkill /IM micbridge.exe /F`

Starting it at sign-in is simpler and costs nothing while idle, so that's the default recommendation.

## Streaming over the Internet

Forward UDP 48020 to the host like you forward Sunshine's ports, or run both machines on the same VPN
(Tailscale, WireGuard, ZeroTier) and use the VPN address as `--host`. Raise `jitterMs` to ~120.

## Building

Needs Rust (stable, MSVC target) plus Visual Studio Build Tools with the "Desktop development with C++"
workload, which provides the compiler, linker and CMake that the bundled libopus needs. If `cmake` is not on
your PATH, add the one that ships with Build Tools:

```
set PATH=C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin;%PATH%
cargo build --release
```

`.cargo/config.toml` sets `CMAKE_POLICY_VERSION_MINIMUM=3.5` because the libopus bundled in `audiopus_sys`
predates CMake 4's minimum-version rule. The exe lands in `target\release\micbridge.exe`.

Crates: [cpal](https://github.com/RustAudio/cpal) for audio I/O, [opus](https://crates.io/crates/opus) (libopus),
[aes-gcm](https://crates.io/crates/aes-gcm), [tray-icon](https://crates.io/crates/tray-icon).

## Limitations

* Windows only for now (both ends). cpal, opus and the protocol are portable; only `win.rs` and the tray
  bits are Windows-specific, so a Linux/macOS client is a modest follow-up.
* One talker at a time. If two clients send, the host keeps the first until it goes quiet for a second.
* The virtual-mic device is something you install once (VB-Cable or Steam's driver); MicBridge does not ship a driver.
