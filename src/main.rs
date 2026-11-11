//! Voice Relay: a Trunk Recorder Pro plugin that relays live calls to digital
//! radios through MMDVM-compatible hotspots. It carries them as DMR today.
//!
//! It is an MMDVM-compatible network master: MMDVM hotspots and repeaters
//! log into it as they would into any DMR network, and each of their slots plays the talkgroups it subscribes to:
//! chosen on the relay's web page, sent in the peer's options
//! (`TS1=…;TS2=…`), or keyed up from a radio (4000 drops it). Every talkgroup
//! heard gets a DMR talkgroup ID: its system's base plus its number.
//!
//! Voice goes out as it came in where it can: AMBE+2 (P25 Phase 2, DMR,
//! NXDN) bit for bit, with fresh FEC. P25 Phase 1's IMBE is converted to
//! AMBE+2 by its model parameters, never through PCM. Analog isn't carried.
//!
//! With a recorder that has the `voice` topic, calls play live; with one
//! that hasn't, each plays when it's done, from its `.sdr` frame file.

mod bridge;
mod dmr;
mod mbe_params;
mod mmdvm;
mod roster;
mod vocoder;
mod web;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use trunk_recorder_plugin::{format, topic, CallInfo, Codec, ConcludedCall, Host, Manifest, Outcome, Plugin, Setup, State, VoiceChunk};

use bridge::{Bridge, Feed, FeedKey, PeerSlots, TICK};
use mmdvm::{FromPeer, Master};
use roster::Roster;

#[derive(Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase", default)]
struct Config {
    /// Relay page
    ///
    /// Its peers, what each slot plays, the calls on the air, and the talkgroups to pick from.
    #[schemars(extend("readOnly" = true, "format" = "uri", "x-open" = "Open the relay page"))]
    relay_page: String,
    /// Master port
    ///
    /// The UDP address hotspots log into (their DMR network's address and port).
    master: String,
    /// Password
    ///
    /// What the hotspots give as their network password.
    #[schemars(extend("x-secret" = true))]
    password: String,
    /// Web page
    ///
    /// Where the relay's page is served (address and port).
    web: String,
    /// Page address
    ///
    /// The name or address browsers reach this machine by, for the link to the relay's page. Empty: this machine's name.
    page_host: String,
    /// Calls play
    ///
    /// `auto`: live when the recorder sends voice frames, else each call once it's done.
    #[schemars(extend("enum" = ["auto", "live", "replay"]))]
    source: String,
    /// Convert Phase 1
    ///
    /// Put P25 Phase 1 (IMBE) calls on DMR, converted to AMBE+2 by their vocoder parameters.
    convert_phase1: bool,
    /// Hang time (s)
    ///
    /// After a call, a slot waits this long for its talkgroup before taking another.
    hang_time: f64,
    /// Keyed talkgroups last (min)
    ///
    /// A talkgroup keyed up from a radio is dropped after this long without a call.
    dynamic_minutes: u64,
    /// Finished calls wait (s)
    ///
    /// How long a finished call may wait for a busy slot (calls played once done).
    replay_window: u64,
    /// Live buffer (ms)
    ///
    /// Voice held before a live call starts, so it doesn't run dry.
    preroll_ms: u64,
    /// Talker alias on a group
    ///
    /// What radios show on a slot subscribed to a group (All calls, a system, one of yours): many talkgroups share its ID, so by default the talkgroup's name. `{unit_name}`, `{unit}`, `{tg_tag}`, `{tg}`, `{system}`; `\\n` breaks the line; 27 characters at most.
    alias_group: String,
    /// Talker alias on a talkgroup
    ///
    /// What radios show on a slot subscribed to the talkgroup itself: by default the talker's name (the group's alias when the talker has none).
    alias_talkgroup: String,
    /// All-calls talkgroup
    ///
    /// A slot on this talkgroup plays every call (one at a time). 0: none.
    all_calls_tg: u32,
    /// RF prefix
    ///
    /// What a hotspot puts before a network talkgroup on RF (often 9 on a custom network). The contacts CSV is written with it, so a radio's contacts match what it hears.
    rf_prefix: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            relay_page: String::new(),
            master: "0.0.0.0:62031".into(),
            password: "passw0rd".into(),
            web: "0.0.0.0:8096".into(),
            page_host: String::new(),
            source: "auto".into(),
            convert_phase1: true,
            hang_time: 2.0,
            dynamic_minutes: 15,
            replay_window: 120,
            preroll_ms: 300,
            alias_group: "{tg_tag}".into(),
            alias_talkgroup: "{unit_name}".into(),
            all_calls_tg: bridge::ALL_CALLS,
            rf_prefix: String::new(),
        }
    }
}

/// One system's choices, made on the relay page (a system not listed takes the defaults).
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemSetting {
    pub system: String,
    /// Left out.
    pub skip: bool,
    /// Its talkgroup N goes out as DMR talkgroup base + N (None: 100000 × its place in the list).
    pub tg_base: Option<u32>,
}

/// JSON kept in the data folder, read back (missing or unreadable: the default).
fn read_json<T: serde::de::DeserializeOwned + Default>(path: &std::path::Path) -> T {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Kept in the data folder (written whole, then moved into place).
fn write_json(path: &std::path::Path, v: &impl Serialize) {
    let tmp = path.with_extension("tmp");
    if let Ok(b) = serde_json::to_vec_pretty(v) {
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// What the threads share.
pub struct Shared {
    pub bridge: Mutex<Bridge>,
    pub roster: Mutex<Roster>,
    pub master: Mutex<Master>,
    /// Talkgroups chosen on the page, per peer and slot.
    pub subs: Mutex<BTreeMap<u32, [Vec<u32>; 2]>>,
    subs_path: PathBuf,
    /// The groups made on the page (kept in `groups_path`).
    pub groups: Mutex<Vec<bridge::GroupSetting>>,
    groups_path: PathBuf,
    /// Each system's choices made on the page (kept in `systems_path`; they take a restart).
    pub systems: Mutex<Vec<SystemSetting>>,
    systems_path: PathBuf,
    /// Every system the recorder has: short name and kind, in its order.
    pub all_systems: Vec<(String, String)>,
    /// The all-calls talkgroup (0: none).
    all_calls: AtomicU32,
    pub rf_prefix: Mutex<String>,
    pub host: Host,
    stop: AtomicBool,
    /// The recorder sends voice frames (calls play live).
    pub live: AtomicBool,
    pub mode: String,
    pub master_addr: String,
}

impl Shared {
    pub fn save_subs(&self) {
        write_json(&self.subs_path, &*self.subs.lock().unwrap());
    }

    /// A system's choices changed on the page: kept, and the plugin restarted for them
    /// (its talkgroups' DMR IDs are set up once).
    pub fn set_system(&self, s: SystemSetting) {
        let mut all = self.systems.lock().unwrap();
        all.retain(|x| x.system != s.system);
        if s.skip || s.tg_base.is_some() {
            all.push(s);
        }
        write_json(&self.systems_path, &*all);
        drop(all);
        self.host.info("restarting for the systems changed on the relay page");
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            std::process::exit(0);
        });
    }

    /// Why `groups` can't be the groups (None: they can).
    pub fn check_groups(&self, groups: &[bridge::GroupSetting]) -> Option<String> {
        let all = self.all_calls.load(Ordering::Relaxed);
        let r = self.roster.lock().unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for g in groups {
            let what = if g.name.is_empty() { format!("Group {}", g.id) } else { format!("Group {}", g.name) };
            if g.id == 0 || g.id > 0xFF_FFFF || g.id == bridge::DISCONNECT_TG {
                return Some(format!("{what}: its DMR talkgroup is 1 to 16777215, not 4000"));
            }
            if g.id == all || r.bases.values().any(|b| *b == g.id) {
                return Some(format!("{what}: {} is a built-in group's", g.id));
            }
            if r.knows(g.id) {
                return Some(format!("{what}: {} is already a talkgroup's", g.id));
            }
            if !seen.insert(g.id) {
                return Some(format!("{what}: another group has {} too", g.id));
            }
            for m in &g.members {
                if let Err(e) = trunk_recorder_plugin::filter::talkgroup_list(&m.talkgroups) {
                    return Some(format!("{what}: {e}"));
                }
            }
        }
        None
    }

    /// The groups: the built-in ones (every call, each system's) and `groups`.
    pub fn set_groups(&self, groups: Vec<bridge::GroupSetting>) {
        let all = self.all_calls.load(Ordering::Relaxed);
        let bases = self.roster.lock().unwrap().bases.clone();
        let mut b = self.bridge.lock().unwrap();
        b.groups.clear();
        if all != 0 {
            b.groups.insert(all, bridge::Group { id: all, name: "All calls".into(), all: true, builtin: true, ..Default::default() });
        }
        for (sys, base) in &bases {
            let id = *base;
            if id != 0 && !b.groups.contains_key(&id) {
                let members = vec![bridge::Member { system: sys.clone(), talkgroups: Vec::new() }];
                b.groups.insert(id, bridge::Group { id, name: format!("All {sys}"), members, builtin: true, ..Default::default() });
            }
        }
        for g in &groups {
            b.groups.entry(g.id).or_insert_with(|| g.group());
        }
        drop(b);
        *self.groups.lock().unwrap() = groups;
    }

    /// Groups edited on the page: checked, used, and kept.
    pub fn edit_groups(&self, groups: Vec<bridge::GroupSetting>) -> Result<(), String> {
        if let Some(e) = self.check_groups(&groups) {
            return Err(e);
        }
        write_json(&self.groups_path, &groups);
        self.set_groups(groups);
        Ok(())
    }

    /// Whether a talkgroup keyed from a radio is one to subscribe to.
    pub fn knows(&self, dmr: u32) -> bool {
        self.bridge.lock().unwrap().groups.contains_key(&dmr) || self.roster.lock().unwrap().knows(dmr)
    }

    /// The page's choice for a peer's slots, into the bridge.
    pub fn apply_subs(&self, peer: u32) {
        let subs = self.subs.lock().unwrap().get(&peer).cloned().unwrap_or_default();
        let mut b = self.bridge.lock().unwrap();
        for (i, tgs) in subs.into_iter().enumerate() {
            b.slot(peer, i as u8 + 1).statics = tgs;
        }
    }

    pub fn peers(&self) -> Vec<PeerSlots> {
        let m = self.master.lock().unwrap();
        m.peers
            .values()
            .filter(|p| p.login == mmdvm::Login::Running)
            .map(|p| PeerSlots { id: p.id, color_code: p.config.color_code, slots: p.config.time_slots() })
            .collect()
    }
}

struct VoiceRelay {
    host: Host,
    shared: Arc<Shared>,
    capture_dir: PathBuf,
    /// The settings it runs with.
    config: Config,
    /// Systems relayed (by short name).
    systems: BTreeMap<String, ()>,
    /// IMBE → AMBE+2 converters, one per live call.
    converters: BTreeMap<FeedKey, vocoder::ImbeToAmbe>,
    convert_phase1: bool,
    threads: Vec<std::thread::JoinHandle<()>>,
    last_status: Instant,
}

/// Each relayed system's DMR talkgroup base and kind, by short name.
fn system_map(chosen: &[SystemSetting], systems: &[(String, String)]) -> (BTreeMap<String, u32>, BTreeMap<String, String>) {
    let mut bases = BTreeMap::new();
    let mut kinds = BTreeMap::new();
    for (i, (name, kind)) in systems.iter().enumerate() {
        let set = chosen.iter().find(|x| &x.system == name);
        if set.is_some_and(|x| x.skip) {
            continue;
        }
        let base = set.and_then(|x| x.tg_base).unwrap_or((i as u32 + 1) * 100_000);
        bases.insert(name.clone(), base);
        kinds.insert(name.clone(), kind.clone());
    }
    (bases, kinds)
}

fn bridge_settings(c: &Config) -> bridge::Settings {
    bridge::Settings {
        hang: Duration::from_secs_f64(c.hang_time.max(0.0)),
        dynamic_timeout: Duration::from_secs(c.dynamic_minutes.max(1) * 60),
        replay_window: Duration::from_secs(c.replay_window),
        preroll: (c.preroll_ms / 20) as usize,
        alias_group: c.alias_group.clone(),
        alias_talkgroup: c.alias_talkgroup.clone(),
    }
}

/// The relay page's address, for the link in its settings.
fn page_url(c: &Config) -> String {
    let (ip, port) = c.web.rsplit_once(':').unwrap_or(("", "8096"));
    let ip = ip.trim_start_matches('[').trim_end_matches(']');
    let host = if !c.page_host.trim().is_empty() {
        c.page_host.trim().to_string()
    } else if !ip.is_empty() && ip != "0.0.0.0" && ip != "::" {
        if ip.contains(':') {
            format!("[{ip}]")
        } else {
            ip.to_string()
        }
    } else {
        std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string()).ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "localhost".into())
    };
    format!("http://{host}:{port}/")
}

/// Only digits, at most two: what a hotspot puts before a network talkgroup.
fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).take(2).collect()
}

impl Plugin for VoiceRelay {
    type Config = Config;
    type SystemConfig = trunk_recorder_plugin::NoConfig;

    fn manifest() -> Manifest {
        Manifest {
            name: "Voice Relay".into(),
            subscribe: vec![topic::CALL_START.into(), topic::CALL_UPDATE.into(), topic::CALL_END.into(), topic::CALL_CONCLUDED.into(), topic::VOICE.into()],
            // (The recorder makes one anyway: asking for it spares a WAV for each call.)
            audio_formats: vec![format::M4A.into()],
            page: Some(trunk_recorder_plugin::Page { title: "Voice Relay".into() }),
            live_settings: true,
            ..trunk_recorder_plugin::manifest!()
        }
    }

    fn start(host: Host, setup: Setup<Config, trunk_recorder_plugin::NoConfig>) -> Result<Self, String> {
        let c = setup.config.clone();
        if !["auto", "live", "replay"].contains(&c.source.as_str()) {
            return Err(format!("Calls play: {} isn't auto, live or replay", c.source));
        }
        let all_systems: Vec<(String, String)> = setup.systems.iter().map(|s| (s.short_name.clone(), s.kind.clone())).collect();
        let systems_path = setup.data_dir.join("systems.json");
        let chosen: Vec<SystemSetting> = read_json(&systems_path);
        let groups_path = setup.data_dir.join("groups.json");
        let groups: Vec<bridge::GroupSetting> = read_json(&groups_path);
        let (bases, kinds) = system_map(&chosen, &all_systems);
        let systems = bases.keys().map(|k| (k.clone(), ())).collect();
        let roster = Roster::load(setup.data_dir.join("roster.json"), bases, kinds, c.convert_phase1);
        let master = Master::bind(&c.master, &c.password).map_err(|e| format!("Master port {}: {e}", c.master))?;
        let sock = master.try_clone_socket().map_err(|e| e.to_string())?;
        let subs_path = setup.data_dir.join("subscriptions.json");
        let subs: BTreeMap<u32, [Vec<u32>; 2]> = std::fs::read(&subs_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let shared = Arc::new(Shared {
            bridge: Mutex::new(Bridge::new(bridge_settings(&c))),
            roster: Mutex::new(roster),
            master: Mutex::new(master),
            subs: Mutex::new(subs),
            subs_path,
            groups: Mutex::new(Vec::new()),
            groups_path,
            systems: Mutex::new(chosen),
            systems_path,
            all_systems,
            all_calls: AtomicU32::new(c.all_calls_tg),
            rf_prefix: Mutex::new(digits(&c.rf_prefix)),
            host: host.clone(),
            stop: AtomicBool::new(false),
            live: AtomicBool::new(c.source == "live"),
            mode: c.source.clone(),
            master_addr: c.master.clone(),
        });
        // (A group that no longer fits, as a system's base moved onto it, is left out and said so.)
        let (ok, bad): (Vec<_>, Vec<_>) = groups.into_iter().partition(|g| shared.check_groups(std::slice::from_ref(g)).is_none());
        for g in &bad {
            host.warn(format!("group {} {} left out: {}", g.id, g.name, shared.check_groups(std::slice::from_ref(g)).unwrap_or_default()));
        }
        shared.set_groups(ok);
        let server = tiny_http::Server::http(&c.web).map_err(|e| format!("Web page {}: {e}", c.web))?;
        let threads = vec![spawn_net(shared.clone(), sock, host.clone()), spawn_tick(shared.clone()), web::spawn(shared.clone(), server)];
        let url = page_url(&c);
        host.info(format!("master on {}, page on {url}", c.master));
        host.status(State::Ok, "no peers yet");
        if c.relay_page != url {
            host.save_settings(&serde_json::json!({ "relayPage": url }));
        }
        Ok(VoiceRelay {
            host,
            shared,
            capture_dir: setup.capture_dir,
            convert_phase1: c.convert_phase1,
            config: c,
            systems,
            converters: BTreeMap::new(),
            threads,
            last_status: Instant::now(),
        })
    }

    fn settings(&mut self, setup: Setup<Config, trunk_recorder_plugin::NoConfig>) -> Result<(), String> {
        let c = setup.config;
        if c == self.config {
            return Ok(());
        }
        // Ports, sockets and the systems' talkgroup IDs are set up once: a restart takes those.
        let old = &self.config;
        let _ = setup.systems;
        let restart = c.master != old.master || c.web != old.web || c.source != old.source;
        if restart {
            self.host.info("restarting for the new settings (ports or how calls play)");
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                std::process::exit(0);
            });
            self.config = c;
            return Ok(());
        }
        self.shared.all_calls.store(c.all_calls_tg, Ordering::Relaxed);
        let groups = self.shared.groups.lock().unwrap().clone();
        if let Some(e) = self.shared.check_groups(&groups) {
            self.shared.all_calls.store(old.all_calls_tg, Ordering::Relaxed);
            return Err(format!("All-calls talkgroup: {e}"));
        }
        self.shared.set_groups(groups);
        self.shared.bridge.lock().unwrap().settings = bridge_settings(&c);
        self.shared.master.lock().unwrap().set_password(&c.password);
        *self.shared.rf_prefix.lock().unwrap() = digits(&c.rf_prefix);
        self.convert_phase1 = c.convert_phase1;
        self.shared.roster.lock().unwrap().convert_imbe = c.convert_phase1;
        let url = page_url(&c);
        if c.relay_page != url {
            self.host.save_settings(&serde_json::json!({ "relayPage": url }));
        }
        self.config = c;
        Ok(())
    }

    fn settings_saved(&mut self, _request: u64, ok: bool, message: String) {
        if !ok {
            self.host.warn(format!("couldn't save its settings: {message}"));
        }
    }

    fn call_start(&mut self, c: CallInfo) {
        self.call(&c, false);
    }

    fn call_update(&mut self, c: CallInfo) {
        self.call(&c, false);
    }

    fn call_end(&mut self, c: CallInfo) {
        self.call(&c, true);
    }

    fn voice(&mut self, v: VoiceChunk) {
        if self.shared.mode == "replay" || !self.systems.contains_key(&v.short_name) {
            return;
        }
        self.shared.live.store(true, Ordering::Relaxed);
        let key = FeedKey { system: v.short_name.clone(), call: v.call_id };
        let (dmr, tag, emergency, unit_name) = {
            let mut r = self.shared.roster.lock().unwrap();
            r.talking(&v.short_name, v.call_id, v.unit, &v.unit_name);
            let a = r.active.get(&(v.short_name.clone(), v.call_id));
            let (tag, emergency) = (a.map(|a| a.tag.clone()).unwrap_or_default(), a.is_some_and(|a| a.emergency));
            let name = if v.unit_name.is_empty() { r.unit_name(&v.short_name, v.unit) } else { v.unit_name.clone() };
            (r.dmr_of(&v.short_name, v.talkgroup), tag, emergency, name)
        };
        let Some(dmr) = dmr else { return };
        let mut frames = Vec::with_capacity(v.frames.len());
        for f in &v.frames {
            match f.codec {
                Codec::Ambe => frames.push(if f.bad() { dmr::silence_frame() } else { bits_of(&f.bits) }),
                Codec::Imbe if self.convert_phase1 => {
                    let conv = self.converters.entry(key.clone()).or_insert_with(vocoder::ImbeToAmbe::new);
                    frames.push(if f.bad() { conv.lost() } else { conv.convert(&bits_of(&f.bits)) });
                }
                Codec::Imbe => {}
            }
        }
        if frames.is_empty() {
            return;
        }
        let mut b = self.shared.bridge.lock().unwrap();
        let feed = b.feeds.entry(key.clone()).or_insert_with(|| Feed::new(key, dmr, v.talkgroup, tag, true));
        feed.emergency |= emergency;
        feed.push(v.unit, &unit_name, frames);
    }

    fn call_concluded(&mut self, k: ConcludedCall) {
        match self.replay(&k) {
            Ok(Some(msg)) => self.host.call_result(&k.path, Outcome::Ok, msg, ""),
            Ok(None) => self.host.call_result(&k.path, Outcome::Skipped, "", ""),
            Err(e) => self.host.call_result(&k.path, Outcome::Skipped, e, ""),
        }
        self.housekeeping();
    }

    fn shutdown(&mut self, _grace: Duration) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.master.lock().unwrap().close_all();
        self.shared.roster.lock().unwrap().save();
        // (The web thread waits in accept: it goes with the process.)
        for t in self.threads.drain(..).take(2) {
            let _ = t.join();
        }
    }
}

impl VoiceRelay {
    fn call(&mut self, c: &CallInfo, end: bool) {
        if !self.systems.contains_key(&c.short_name) {
            return;
        }
        {
            let mut r = self.shared.roster.lock().unwrap();
            if end {
                r.call_end(c);
            } else {
                r.call(c);
            }
        }
        let key = FeedKey { system: c.short_name.clone(), call: c.id };
        if let Some(f) = self.shared.bridge.lock().unwrap().feeds.get_mut(&key) {
            f.emergency |= c.emergency;
            if !c.talkgroup_tag.is_empty() {
                f.tg_tag = c.talkgroup_tag.clone();
            }
            if end && f.live {
                f.done = true;
            }
        }
        if end {
            self.converters.remove(&key);
        }
        self.housekeeping();
    }

    /// A finished call, played from its frame file (when calls aren't live).
    fn replay(&mut self, k: &ConcludedCall) -> Result<Option<String>, String> {
        let c = &k.call;
        if self.shared.live.load(Ordering::Relaxed) || !self.systems.contains_key(&c.short_name) {
            return Ok(None);
        }
        if c.encrypted && !c.extra.get("decrypted").is_some_and(|v| v.as_i64() == Some(1) || v.as_bool() == Some(true)) {
            return Err("encrypted".into());
        }
        let Some(dmr) = self.shared.roster.lock().unwrap().dmr_of(&c.short_name, c.talkgroup) else { return Ok(None) };
        let path = self.capture_dir.join(format!("{}.sdr", k.path));
        let bytes = std::fs::read(&path).map_err(|_| "no frame file (analog, or frames not kept)".to_string())?;
        let frames = trunk_core::trunk::sdr::sdr_frames(&bytes);
        let mut conv = vocoder::ImbeToAmbe::new();
        let mut ambe = Vec::with_capacity(frames.len());
        for f in &frames {
            match f.codec {
                Codec::Ambe => ambe.push(if f.bad() { dmr::silence_frame() } else { bits_of(&f.bits) }),
                Codec::Imbe if self.convert_phase1 => {
                    ambe.push(if f.bad() { conv.lost() } else { conv.convert(&bits_of(&f.bits)) });
                }
                Codec::Imbe => return Err("Phase 1 (conversion off)".into()),
            }
        }
        if ambe.is_empty() {
            return Ok(None);
        }
        {
            let mut r = self.shared.roster.lock().unwrap();
            let voice = match (frames.first().map(|f| f.codec), r.kinds.get(&c.short_name).map(String::as_str)) {
                (Some(Codec::Imbe), Some("edacs")) => "ProVoice",
                (Some(Codec::Imbe), _) => "Phase 1",
                (_, Some("dmr")) => "DMR",
                (_, Some("nxdn")) => "NXDN",
                _ => "Phase 2",
            };
            r.heard_tg(&c.short_name, c.talkgroup, &c.talkgroup_tag, voice);
        }
        let key = FeedKey { system: c.short_name.clone(), call: c.call_num as u32 };
        let mut feed = Feed::new(key.clone(), dmr, c.talkgroup, c.talkgroup_tag.clone(), false);
        feed.emergency = c.emergency;
        // Each radio from where it starts talking (the first from the start).
        let mut talkers: Vec<(usize, u32, String)> =
            c.src_list.iter().filter(|s| s.src > 0).map(|s| ((s.pos / 0.02).round() as usize, s.src as u32, s.tag.clone())).collect();
        talkers.sort_by_key(|t| t.0);
        let n = ambe.len();
        {
            let mut r = self.shared.roster.lock().unwrap();
            for (i, (_, unit, name)) in talkers.iter().enumerate() {
                r.heard_unit(&c.short_name, *unit, name);
                let start = if i == 0 { 0 } else { talkers[i].0.min(n) };
                let end = talkers.get(i + 1).map_or(n, |t| t.0.min(n)).max(start);
                let name = if name.is_empty() { r.unit_name(&c.short_name, *unit) } else { name.clone() };
                feed.push(*unit, &name, ambe[start..end].iter().copied());
            }
        }
        if talkers.is_empty() {
            feed.push(0, "", ambe);
        }
        feed.done = true;
        self.shared.bridge.lock().unwrap().feeds.insert(key, feed);
        Ok(Some(format!("queued {:.1} s as TG {dmr}", n as f64 * 0.02)))
    }

    fn housekeeping(&mut self) {
        if self.last_status.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_status = Instant::now();
        let peers = self.shared.peers();
        let playing = self.shared.bridge.lock().unwrap().playing();
        let mode = if self.shared.live.load(Ordering::Relaxed) { "live" } else { "after each call" };
        let names: Vec<String> = {
            let m = self.shared.master.lock().unwrap();
            peers.iter().filter_map(|p| m.peers.get(&p.id).map(|x| x.config.callsign.clone())).collect()
        };
        let msg = match peers.len() {
            0 => format!("no peers ({mode})"),
            n => format!("{n} peer(s): {}; {playing} slot(s) playing ({mode})", names.join(", ")),
        };
        self.host.status(State::Ok, msg);
        let mut r = self.shared.roster.lock().unwrap();
        r.prune_active();
        r.save();
    }
}

/// Peers' packets: logins, options, and their radios keying up.
fn spawn_net(shared: Arc<Shared>, sock: std::net::UdpSocket, host: Host) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        let mut swept = Instant::now();
        while !shared.stop.load(Ordering::Relaxed) {
            if swept.elapsed() > Duration::from_secs(5) {
                swept = Instant::now();
                let gone = shared.master.lock().unwrap().expire();
                for id in gone {
                    let mut b = shared.bridge.lock().unwrap();
                    b.peer_gone(id);
                    b.note(format!("{id} timed out"));
                    host.info(format!("peer {id} timed out"));
                }
            }
            let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
            let r = shared.master.lock().unwrap().handle(&buf[..n], from);
            match r {
                Some(FromPeer::Up(id)) => {
                    let (call, sw) = {
                        let m = shared.master.lock().unwrap();
                        m.peers.get(&id).map(|p| (p.config.callsign.clone(), p.config.software.clone())).unwrap_or_default()
                    };
                    shared.apply_subs(id);
                    shared.bridge.lock().unwrap().note(format!("{id} {call} logged in from {from} ({sw})"));
                    host.info(format!("peer {id} {call} logged in from {from}"));
                }
                Some(FromPeer::Options(id, o)) => {
                    let [ts1, ts2] = mmdvm::parse_options(&o);
                    let mut b = shared.bridge.lock().unwrap();
                    b.slot(id, 1).options = ts1;
                    b.slot(id, 2).options = ts2;
                    b.note(format!("{id} options: {o}"));
                }
                Some(FromPeer::Dmrd(id, d)) => {
                    let known = shared.knows(d.dst);
                    shared.bridge.lock().unwrap().keyed(id, &d, |_| known);
                }
                Some(FromPeer::Closed(id)) => {
                    let mut b = shared.bridge.lock().unwrap();
                    b.peer_gone(id);
                    b.note(format!("{id} logged out"));
                }
                None => {}
            }
        }
    })
}

/// A burst for every playing slot, every 60 ms.
fn spawn_tick(shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut next = Instant::now();
        while !shared.stop.load(Ordering::Relaxed) {
            next += TICK;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > TICK * 10 {
                // Far behind (suspended): start again from now.
                next = now;
            }
            let peers = shared.peers();
            if peers.is_empty() {
                continue;
            }
            let out = shared.bridge.lock().unwrap().tick(&peers);
            if out.is_empty() {
                continue;
            }
            let m = shared.master.lock().unwrap();
            for (peer, d) in &out {
                m.send_dmrd(*peer, d);
            }
        }
    })
}

fn main() {
    trunk_recorder_plugin::run::<VoiceRelay>();
}

/// A frame's information bits as a fixed-size array (short: zero-padded).
fn bits_of<const N: usize>(bits: &[u8]) -> [u8; N] {
    std::array::from_fn(|i| bits.get(i).copied().unwrap_or(0) & 1)
}
