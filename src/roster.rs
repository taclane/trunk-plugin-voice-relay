//! The roster: every talkgroup heard, its DMR talkgroup ID, the radios
//! heard and their names, and the calls going on now. Kept in the plugin's
//! data folder across restarts.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use trunk_recorder_plugin::CallInfo;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TgEntry {
    pub system: String,
    pub tg: u32,
    pub dmr: u32,
    pub tag: String,
    /// Unix seconds.
    pub last: f64,
    pub calls: u64,
    /// "Phase 1", "Phase 2", "DMR", "NXDN", "analog", …
    pub voice: String,
    /// Its voice can go out on DMR (AMBE+2, or IMBE converted).
    pub playable: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UnitEntry {
    pub system: String,
    pub id: u32,
    pub name: String,
    pub last: f64,
}

/// A call heard now, or lately.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Active {
    pub system: String,
    pub call: u32,
    pub tg: u32,
    pub dmr: u32,
    pub tag: String,
    pub unit: u32,
    pub unit_name: String,
    pub start: f64,
    pub end: Option<f64>,
    pub emergency: bool,
    pub encrypted: bool,
    pub voice: String,
    pub playable: bool,
    /// When it was last heard of (Unix s).
    #[serde(skip)]
    pub seen: f64,
}

/// An active call not heard of for this long is taken as over (s): its end was missed.
const ACTIVE_STALE_S: f64 = 60.0;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    talkgroups: Vec<TgEntry>,
    units: Vec<UnitEntry>,
}

pub struct Roster {
    /// Each system's DMR talkgroup base (its talkgroup N is base + N).
    pub bases: BTreeMap<String, u32>,
    /// Each system's kind ("p25", "dmr", …).
    pub kinds: BTreeMap<String, String>,
    pub tgs: BTreeMap<u32, TgEntry>,
    pub units: BTreeMap<(String, u32), UnitEntry>,
    pub active: BTreeMap<(String, u32), Active>,
    pub recent: VecDeque<Active>,
    /// Phase 1 voice can be converted to AMBE+2.
    pub convert_imbe: bool,
    path: PathBuf,
    pub dirty: bool,
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

impl Roster {
    pub fn load(path: PathBuf, bases: BTreeMap<String, u32>, kinds: BTreeMap<String, String>, convert_imbe: bool) -> Roster {
        let saved: Saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut r = Roster {
            bases,
            kinds,
            tgs: BTreeMap::new(),
            units: BTreeMap::new(),
            active: BTreeMap::new(),
            recent: VecDeque::new(),
            convert_imbe,
            path,
            dirty: false,
        };
        for mut t in saved.talkgroups {
            // A system's base may have moved since.
            let Some(dmr) = r.dmr_of(&t.system, t.tg) else { continue };
            t.dmr = dmr;
            t.playable = r.playable(&t.voice);
            r.tgs.insert(dmr, t);
        }
        for u in saved.units {
            r.units.insert((u.system.clone(), u.id), u);
        }
        r
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let s = Saved { talkgroups: self.tgs.values().cloned().collect(), units: self.units.values().cloned().collect() };
        let tmp = self.path.with_extension("tmp");
        if serde_json::to_vec(&s).ok().and_then(|b| std::fs::write(&tmp, b).ok()).is_some() && std::fs::rename(&tmp, &self.path).is_ok() {
            self.dirty = false;
        }
    }

    /// The DMR talkgroup for a system's talkgroup (None: the system isn't bridged).
    pub fn dmr_of(&self, system: &str, tg: u32) -> Option<u32> {
        let id = self.bases.get(system)?.checked_add(tg)?;
        (id > 0 && id <= 0xFF_FFFF).then_some(id)
    }

    pub fn knows(&self, dmr: u32) -> bool {
        self.tgs.contains_key(&dmr)
    }

    pub fn playable(&self, voice: &str) -> bool {
        match voice {
            "Phase 2" | "DMR" | "NXDN" => true,
            "Phase 1" | "ProVoice" => self.convert_imbe,
            _ => false,
        }
    }

    /// How a call's voice is carried.
    pub fn voice_of(&self, c: &CallInfo) -> String {
        if c.analog {
            return "analog".into();
        }
        match self.kinds.get(&c.short_name).map(String::as_str) {
            Some("dmr") => "DMR".into(),
            Some("nxdn") => "NXDN".into(),
            Some("edacs") => "ProVoice".into(),
            Some("smartnet") => "analog".into(),
            _ if c.tdma_slot.is_some() => "Phase 2".into(),
            _ => "Phase 1".into(),
        }
    }

    pub fn unit_name(&self, system: &str, unit: u32) -> String {
        self.units.get(&(system.to_string(), unit)).map(|u| u.name.clone()).unwrap_or_default()
    }

    /// Note a unit's name (a call's sources, a voice chunk).
    pub fn heard_unit(&mut self, system: &str, unit: u32, name: &str) {
        if unit == 0 {
            return;
        }
        let e = self.units.entry((system.to_string(), unit)).or_insert_with(|| UnitEntry { system: system.into(), id: unit, ..Default::default() });
        if !name.is_empty() && e.name != name {
            e.name = name.to_string();
        }
        e.last = now_s();
        self.dirty = true;
    }

    /// A talkgroup heard on a finished call (when its start wasn't seen).
    pub fn heard_tg(&mut self, system: &str, tg: u32, tag: &str, voice: &str) {
        let Some(dmr) = self.dmr_of(system, tg) else { return };
        let playable = self.playable(voice);
        let t = self.tgs.entry(dmr).or_insert_with(|| TgEntry { system: system.into(), tg, dmr, calls: 1, ..Default::default() });
        if !tag.is_empty() {
            t.tag = tag.to_string();
        }
        if t.voice.is_empty() {
            t.voice = voice.to_string();
            t.playable = playable;
        }
        t.last = t.last.max(now_s());
        self.dirty = true;
    }

    /// A call started or changed.
    pub fn call(&mut self, c: &CallInfo) -> Option<u32> {
        let dmr = self.dmr_of(&c.short_name, c.talkgroup)?;
        let voice = self.voice_of(c);
        let playable = self.playable(&voice) && (!c.encrypted || c.decrypted);
        for s in &c.sources {
            self.heard_unit(&c.short_name, s.unit, &s.tag);
        }
        let new = !self.active.contains_key(&(c.short_name.clone(), c.id));
        let voice_ok = self.playable(&voice);
        let t = self.tgs.entry(dmr).or_insert_with(|| TgEntry { system: c.short_name.clone(), tg: c.talkgroup, dmr, ..Default::default() });
        if !c.talkgroup_tag.is_empty() {
            t.tag = c.talkgroup_tag.clone();
        }
        t.last = now_s();
        if new {
            t.calls += 1;
        }
        // An encrypted call says nothing of how the talkgroup's clear calls sound.
        if !c.encrypted || c.decrypted {
            t.voice = voice.clone();
            t.playable = voice_ok;
        }
        let tag = t.tag.clone();
        self.dirty = true;
        let src = c.sources.last();
        let a = self.active.entry((c.short_name.clone(), c.id)).or_default();
        *a = Active {
            system: c.short_name.clone(),
            call: c.id,
            tg: c.talkgroup,
            dmr,
            tag,
            unit: src.map_or(0, |s| s.unit),
            unit_name: src.map(|s| s.tag.clone()).unwrap_or_default(),
            start: c.start_time,
            end: None,
            emergency: c.emergency,
            encrypted: c.encrypted && !c.decrypted,
            voice,
            playable,
            seen: now_s(),
        };
        Some(dmr)
    }

    /// Calls whose end was missed: over.
    pub fn prune_active(&mut self) {
        let now = now_s();
        let stale: Vec<(String, u32)> = self.active.iter().filter(|(_, a)| now - a.seen > ACTIVE_STALE_S).map(|(k, _)| k.clone()).collect();
        for k in stale {
            if let Some(mut a) = self.active.remove(&k) {
                a.end = Some(a.seen);
                self.recent.push_front(a);
            }
        }
        self.recent.truncate(50);
    }

    pub fn call_end(&mut self, c: &CallInfo) {
        self.call(c);
        if let Some(mut a) = self.active.remove(&(c.short_name.clone(), c.id)) {
            a.end = Some(now_s());
            self.recent.push_front(a);
            self.recent.truncate(50);
        }
    }

    /// The current talker on an active call, from its voice.
    pub fn talking(&mut self, system: &str, call: u32, unit: u32, name: &str) {
        self.heard_unit(system, unit, name);
        let name = if name.is_empty() { self.unit_name(system, unit) } else { name.to_string() };
        if let Some(a) = self.active.get_mut(&(system.to_string(), call)) {
            a.seen = now_s();
            if unit != 0 {
                a.unit = unit;
                a.unit_name = name;
            }
        }
    }

    /// Contacts for a codeplug: groups, talkgroups, then named radios.
    /// `prefix`: what the hotspot puts before a network talkgroup on RF
    /// (often "9" on a custom network), so the radio's IDs match what it hears.
    pub fn contacts_csv(&self, groups: &[(u32, String)], prefix: &str) -> String {
        let mut s = String::from("No.,Name,Call Type,Call ID\n");
        let mut n = 0;
        let clean = |t: &str| t.replace([',', '"'], " ").chars().take(16).collect::<String>();
        let id = |dmr: u32| format!("{prefix}{dmr}");
        for (gid, name) in groups {
            n += 1;
            s += &format!("{n},{},Group Call,{}\n", clean(name), id(*gid));
        }
        for t in self.tgs.values() {
            n += 1;
            let name = if t.tag.is_empty() { format!("{} {}", t.system, t.tg) } else { clean(&t.tag) };
            s += &format!("{n},{name},Group Call,{}\n", id(t.dmr));
        }
        for u in self.units.values().filter(|u| !u.name.is_empty()) {
            n += 1;
            s += &format!("{n},{},Private Call,{}\n", clean(&u.name), u.id);
        }
        s
    }
}
