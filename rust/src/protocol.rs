//! Wire format (UDP), 16-byte header then AES-256-GCM ciphertext + 16-byte tag. Identical to the C# build.
//!
//! ```text
//!   0..1   "MB"
//!   2      version (1)
//!   3      type: 1 = audio (one Opus 20 ms frame), 2 = ping (client keepalive), 3 = pong (host reply)
//!   4..11  session id (random per process start, chosen by the sender)
//!   12..15 sequence number (big endian, per sender)
//! ```
//! Nonce = session id || sequence (12 bytes). Each sender has its own session id so nonces never collide
//! across directions. The header is authenticated as additional data, so a packet with the wrong key or a
//! tampered header is dropped silently.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use std::collections::HashMap;

pub const HEADER_LEN: usize = 16;
pub const TAG_LEN: usize = 16;
pub const VERSION: u8 = 1;
pub const TYPE_AUDIO: u8 = 1;
pub const TYPE_PING: u8 = 2;
pub const TYPE_PONG: u8 = 3;
pub const MAX_PACKET: usize = 1500;

fn write_header(dst: &mut [u8], ptype: u8, session: &[u8; 8], seq: u32) {
    dst[0] = b'M';
    dst[1] = b'B';
    dst[2] = VERSION;
    dst[3] = ptype;
    dst[4..12].copy_from_slice(session);
    dst[12..16].copy_from_slice(&seq.to_be_bytes());
}

pub struct Header {
    pub ptype: u8,
    pub session: u64,
    pub seq: u32,
}

pub fn parse_header(pkt: &[u8]) -> Option<Header> {
    if pkt.len() < HEADER_LEN + TAG_LEN || pkt[0] != b'M' || pkt[1] != b'B' || pkt[2] != VERSION {
        return None;
    }
    Some(Header {
        ptype: pkt[3],
        session: u64::from_be_bytes(pkt[4..12].try_into().unwrap()),
        seq: u32::from_be_bytes(pkt[12..16].try_into().unwrap()),
    })
}

/// Encrypts outgoing packets for one sender (one session id + counter).
pub struct Sealer {
    cipher: Aes256Gcm,
    session: [u8; 8],
    seq: u32,
}

impl Sealer {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { cipher: Aes256Gcm::new(key.into()), session: rand::random(), seq: 0 }
    }

    /// Builds a complete packet.
    pub fn seal(&mut self, ptype: u8, payload: &[u8]) -> Vec<u8> {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let mut pkt = vec![0u8; HEADER_LEN];
        write_header(&mut pkt, ptype, &self.session, seq);
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&self.session);
        nonce[8..].copy_from_slice(&seq.to_be_bytes());
        let ct = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: payload, aad: &pkt[..HEADER_LEN] })
            .expect("AES-GCM encrypt cannot fail for in-range sizes");
        pkt.extend_from_slice(&ct);
        pkt
    }
}

/// Authenticates and decrypts incoming packets, with a sliding anti-replay window per sender session.
pub struct Opener {
    cipher: Aes256Gcm,
    windows: HashMap<u64, ReplayWindow>,
}

impl Opener {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { cipher: Aes256Gcm::new(key.into()), windows: HashMap::new() }
    }

    /// Returns the header and decrypted payload, or None if the packet is invalid or replayed.
    pub fn open(&mut self, pkt: &[u8]) -> Option<(Header, Vec<u8>)> {
        let hdr = parse_header(pkt)?;
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&pkt[4..12]);
        nonce[8..].copy_from_slice(&pkt[12..16]);
        let plain = self
            .cipher
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: &pkt[HEADER_LEN..], aad: &pkt[..HEADER_LEN] })
            .ok()?;
        // Only authenticated packets touch replay state, so garbage can't pollute it.
        if self.windows.len() > 64 {
            self.windows.clear();
        }
        let win = self.windows.entry(hdr.session).or_default();
        if !win.accept(hdr.seq) {
            return None;
        }
        Some((hdr, plain))
    }
}

/// 128-packet sliding bitmap, like IPsec/DTLS replay protection.
#[derive(Default)]
struct ReplayWindow {
    any: bool,
    max: u32,
    bits: u128, // bit i set => seq (max - i) seen
}

impl ReplayWindow {
    fn accept(&mut self, seq: u32) -> bool {
        if !self.any {
            self.any = true;
            self.max = seq;
            self.bits = 1;
            return true;
        }
        if seq > self.max {
            let shift = seq - self.max;
            self.bits = if shift >= 128 { 1 } else { (self.bits << shift) | 1 };
            self.max = seq;
            return true;
        }
        let back = self.max - seq;
        if back >= 128 {
            return false;
        }
        let m = 1u128 << back;
        if self.bits & m != 0 {
            return false;
        }
        self.bits |= m;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_replay() {
        let key = [7u8; 32];
        let mut s = Sealer::new(&key);
        let mut o = Opener::new(&key);
        let p1 = s.seal(TYPE_AUDIO, b"hello");
        let p2 = s.seal(TYPE_PING, b"");
        let (h, body) = o.open(&p1).unwrap();
        assert_eq!(h.ptype, TYPE_AUDIO);
        assert_eq!(body, b"hello");
        assert!(o.open(&p1).is_none(), "replay must be rejected");
        assert!(o.open(&p2).is_some());
        let mut bad = p2.clone();
        bad[20] ^= 1;
        assert!(o.open(&bad).is_none());
        let mut wrong = Opener::new(&[8u8; 32]);
        assert!(wrong.open(&s.seal(TYPE_AUDIO, b"x")).is_none());
    }
}
