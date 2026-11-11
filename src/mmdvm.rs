//! The master side of the MMDVM network protocol: what MMDVM hotspots and
//! repeaters log into as their DMR network.
//!
//! ```text
//! peer → RPTL id                 master → RPTACK salt
//! peer → RPTK id sha256(salt‖pw) master → RPTACK id
//! peer → RPTC id config (302)    master → RPTACK id
//! peer → RPTO id options         master → RPTACK id     (optional, any time)
//! peer → RPTPING id              master → MSTPONG id
//! peer → RPTCL id                master → MSTCL id      (closing)
//! either way: DMRD …             master → MSTNAK id     (not logged in)
//! ```

use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::dmr::Dmrd;

/// A peer not heard from (pings every 5–10 s) for this long is gone.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Login {
    /// Sent the salt, waiting for RPTK.
    Challenged(u32),
    /// Authenticated, waiting for RPTC.
    Authed,
    /// Configured: traffic flows.
    Running,
}

/// What a peer told us of itself (RPTC), trimmed.
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerConfig {
    pub callsign: String,
    pub rx_hz: u64,
    pub tx_hz: u64,
    pub power: String,
    pub color_code: u8,
    pub latitude: String,
    pub longitude: String,
    pub location: String,
    pub description: String,
    /// '1' TS1, '2' TS2, '3' both, '4' simplex DMO (as hotspots write it).
    pub slots: String,
    pub url: String,
    pub software: String,
    pub package: String,
}

impl PeerConfig {
    fn parse(c: &[u8]) -> PeerConfig {
        let f = |a: usize, b: usize| c.get(a..b.min(c.len())).map(|s| String::from_utf8_lossy(s).trim().trim_matches('\0').to_string()).unwrap_or_default();
        PeerConfig {
            callsign: f(0, 8),
            rx_hz: f(8, 17).parse().unwrap_or(0),
            tx_hz: f(17, 26).parse().unwrap_or(0),
            power: f(26, 28),
            color_code: f(28, 30).parse().unwrap_or(1),
            latitude: f(30, 38),
            longitude: f(38, 47),
            location: f(50, 70),
            description: f(70, 89),
            slots: f(89, 90),
            url: f(90, 214),
            software: f(214, 254),
            package: f(254, 294),
        }
    }

    /// The slots this peer carries: a simplex hotspot only TS2.
    pub fn time_slots(&self) -> Vec<u8> {
        match self.slots.as_str() {
            "1" => vec![1],
            "2" | "4" => vec![2],
            _ => vec![1, 2],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Peer {
    pub id: u32,
    pub addr: SocketAddr,
    pub login: Login,
    pub config: PeerConfig,
    /// RPTO, as sent.
    pub options: String,
    pub since: f64,
    pub seen: Instant,
}

/// What a peer's packet asks of us, once the protocol part is answered.
#[derive(Debug, PartialEq)]
pub enum FromPeer {
    /// It finished logging in (or came back).
    Up(u32),
    /// It sent options: `TS1=…;TS2=…` and the like.
    Options(u32, String),
    /// A burst from the peer's radio.
    Dmrd(u32, Dmrd),
    Closed(u32),
}

pub struct Master {
    sock: UdpSocket,
    password: String,
    pub peers: BTreeMap<u32, Peer>,
}

fn now_s() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn salt() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.finish() as u32
}

pub fn auth_hash(salt: u32, password: &str) -> [u8; 32] {
    let mut s = Sha256::new();
    s.update(salt.to_be_bytes());
    s.update(password.as_bytes());
    s.finalize().into()
}

fn msg(tag: &[u8], id: u32) -> Vec<u8> {
    let mut b = tag.to_vec();
    b.extend_from_slice(&id.to_be_bytes());
    b
}

impl Master {
    pub fn bind(addr: &str, password: &str) -> std::io::Result<Master> {
        let sock = UdpSocket::bind(addr)?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        Ok(Master { sock, password: password.to_string(), peers: BTreeMap::new() })
    }

    /// The password peers log in with from now on (those logged in stay).
    pub fn set_password(&mut self, password: &str) {
        self.password = password.to_string();
    }

    pub fn try_clone_socket(&self) -> std::io::Result<UdpSocket> {
        self.sock.try_clone()
    }

    /// Wait (up to the socket's timeout) for one packet; answer the protocol
    /// part and say what's left for the bridge.
    pub fn recv(&mut self) -> Option<FromPeer> {
        let mut buf = [0u8; 512];
        let (n, from) = self.sock.recv_from(&mut buf).ok()?;
        self.handle(&buf[..n], from)
    }

    fn send(&self, to: SocketAddr, b: &[u8]) {
        let _ = self.sock.send_to(b, to);
    }

    /// One packet from `from`.
    pub fn handle(&mut self, b: &[u8], from: SocketAddr) -> Option<FromPeer> {
        let id_at = |i: usize| b.get(i..i + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()));
        if b.starts_with(b"DMRD") {
            let d = Dmrd::decode(b)?;
            return match self.running(d.peer, from) {
                true => Some(FromPeer::Dmrd(d.peer, d)),
                false => {
                    self.send(from, &msg(b"MSTNAK", d.peer));
                    None
                }
            };
        }
        if b.starts_with(b"RPTL") {
            let id = id_at(4)?;
            let s = salt();
            self.peers.insert(
                id,
                Peer {
                    id,
                    addr: from,
                    login: Login::Challenged(s),
                    config: PeerConfig::default(),
                    options: String::new(),
                    since: now_s(),
                    seen: Instant::now(),
                },
            );
            self.send(from, &msg(b"RPTACK", s));
            return None;
        }
        if b.starts_with(b"RPTK") {
            let id = id_at(4)?;
            let ok = match self.peers.get(&id) {
                Some(p) if p.addr == from => match p.login {
                    Login::Challenged(s) => b.get(8..40) == Some(&auth_hash(s, &self.password)[..]),
                    _ => false,
                },
                _ => false,
            };
            if ok {
                let p = self.peers.get_mut(&id).unwrap();
                p.login = Login::Authed;
                p.seen = Instant::now();
                self.send(from, &msg(b"RPTACK", id));
            } else {
                self.peers.remove(&id);
                self.send(from, &msg(b"MSTNAK", id));
            }
            return None;
        }
        if b.starts_with(b"RPTCL") {
            let id = id_at(5)?;
            if self.peers.get(&id).is_some_and(|p| p.addr == from) {
                self.peers.remove(&id);
                return Some(FromPeer::Closed(id));
            }
            return None;
        }
        // RPTC (config) and RPTO (options) share a prefix with RPTCL above.
        if b.starts_with(b"RPTC") {
            let id = id_at(4)?;
            match self.peers.get_mut(&id) {
                Some(p) if p.addr == from && matches!(p.login, Login::Authed | Login::Running) => {
                    p.config = PeerConfig::parse(&b[8..]);
                    p.login = Login::Running;
                    p.seen = Instant::now();
                    self.send(from, &msg(b"RPTACK", id));
                    return Some(FromPeer::Up(id));
                }
                _ => {
                    self.send(from, &msg(b"MSTNAK", id));
                    return None;
                }
            }
        }
        if b.starts_with(b"RPTO") {
            let id = id_at(4)?;
            if !self.running(id, from) {
                self.send(from, &msg(b"MSTNAK", id));
                return None;
            }
            let o = String::from_utf8_lossy(&b[8..]).trim_matches('\0').trim().to_string();
            self.peers.get_mut(&id).unwrap().options = o.clone();
            self.send(from, &msg(b"RPTACK", id));
            return Some(FromPeer::Options(id, o));
        }
        if b.starts_with(b"RPTPING") {
            let id = id_at(7)?;
            if self.running(id, from) {
                self.send(from, &msg(b"MSTPONG", id));
            } else {
                self.send(from, &msg(b"MSTNAK", id));
            }
            return None;
        }
        // Anything else a peer sends (talker alias DMRA, GPS DMRG, beacons)
        // still says it's alive.
        if let Some(id) = id_at(4) {
            self.running(id, from);
        }
        None
    }

    /// Whether `id` is logged in from `from`, noting that it's alive.
    fn running(&mut self, id: u32, from: SocketAddr) -> bool {
        match self.peers.get_mut(&id) {
            Some(p) if p.addr == from && p.login == Login::Running => {
                p.seen = Instant::now();
                true
            }
            _ => false,
        }
    }

    /// Drop peers gone quiet; returns their IDs.
    pub fn expire(&mut self) -> Vec<u32> {
        let gone: Vec<u32> = self.peers.values().filter(|p| p.seen.elapsed() > PEER_TIMEOUT).map(|p| p.id).collect();
        for id in &gone {
            self.peers.remove(id);
        }
        gone
    }

    /// A burst to a running peer (its ID goes in the packet).
    pub fn send_dmrd(&self, peer: u32, d: &Dmrd) -> bool {
        match self.peers.get(&peer) {
            Some(p) if p.login == Login::Running => {
                let mut d = d.clone();
                d.peer = peer;
                self.send(p.addr, &d.encode());
                true
            }
            _ => false,
        }
    }

    /// Tell every peer we're closing.
    pub fn close_all(&mut self) {
        for p in self.peers.values() {
            self.send(p.addr, &msg(b"MSTCL", p.id));
        }
        self.peers.clear();
    }
}

/// `TS1=1,2;TS2=91;…` → the talkgroups for each slot (other keys ignored).
pub fn parse_options(o: &str) -> [Vec<u32>; 2] {
    let mut out = [Vec::new(), Vec::new()];
    for part in o.split(';') {
        let Some((k, v)) = part.split_once('=') else { continue };
        let slot = match k.trim().to_ascii_uppercase().as_str() {
            "TS1" => 0,
            "TS2" => 1,
            _ => continue,
        };
        out[slot] = v.split(',').filter_map(|t| t.trim().parse().ok()).filter(|&t| t > 0).collect();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_config(slots: &str) -> Vec<u8> {
        let mut c = format!(
            "{:<8}{:09}{:09}{:02}{:02}{:<8}{:<9}{:03}{:<20}{:<19}{:1}{:<124}{:<40}{:<40}",
            "N0CALL", 433450000u64, 433450000u64, 1, 1, "41.7", "-72.7", 0, "Kitchen", "Hotspot", slots, "", "MMDVM_HS", "Hotspot"
        )
        .into_bytes();
        c.truncate(302 - 8);
        c
    }

    #[test]
    fn a_peer_logs_in_and_pings() {
        // A loopback pair: the master and a peer socket.
        let mut m = Master::bind("127.0.0.1:0", "s3cret").unwrap();
        let me = m.sock.local_addr().unwrap();
        let p = UdpSocket::bind("127.0.0.1:0").unwrap();
        p.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let from = p.local_addr().unwrap();
        let id = 3_112_345u32;
        let mut buf = [0u8; 64];
        let mut ask = |m: &mut Master, b: Vec<u8>| -> (Option<FromPeer>, Vec<u8>) {
            let r = m.handle(&b, from);
            let n = p.recv(&mut buf).unwrap();
            (r, buf[..n].to_vec())
        };
        let (_, r) = ask(&mut m, msg(b"RPTL", id));
        assert_eq!(&r[..6], b"RPTACK");
        let s = u32::from_be_bytes(r[6..10].try_into().unwrap());
        // A wrong password is refused.
        let mut k = msg(b"RPTK", id);
        k.extend(auth_hash(s, "nope"));
        let (_, r) = ask(&mut m, k);
        assert_eq!(&r[..6], b"MSTNAK");
        let (_, r) = ask(&mut m, msg(b"RPTL", id));
        let s = u32::from_be_bytes(r[6..10].try_into().unwrap());
        let mut k = msg(b"RPTK", id);
        k.extend(auth_hash(s, "s3cret"));
        let (_, r) = ask(&mut m, k);
        assert_eq!(r, msg(b"RPTACK", id));
        let mut c = msg(b"RPTC", id);
        c.extend(peer_config("4"));
        let (up, r) = ask(&mut m, c);
        assert_eq!(up, Some(FromPeer::Up(id)));
        assert_eq!(r, msg(b"RPTACK", id));
        let peer = &m.peers[&id];
        assert_eq!((peer.config.callsign.as_str(), peer.config.rx_hz, peer.config.time_slots()), ("N0CALL", 433_450_000, vec![2]));
        assert_eq!(peer.config.software, "MMDVM_HS");
        let mut o = msg(b"RPTO", id);
        o.extend(b"TS1=108703,4000;TS2=226121;");
        let (opt, _) = ask(&mut m, o);
        assert_eq!(opt, Some(FromPeer::Options(id, "TS1=108703,4000;TS2=226121;".into())));
        let (_, r) = ask(&mut m, msg(b"RPTPING", id));
        assert_eq!(r, msg(b"MSTPONG", id));
        let _ = me;
    }

    #[test]
    fn options_name_each_slots_talkgroups() {
        assert_eq!(parse_options("TS1=1,2, 3;TS2=91;DIAL=0"), [vec![1, 2, 3], vec![91]]);
        assert_eq!(parse_options("ts2=108703"), [Vec::<u32>::new(), vec![108703]]);
        assert_eq!(parse_options(""), [Vec::<u32>::new(), Vec::new()]);
    }
}
