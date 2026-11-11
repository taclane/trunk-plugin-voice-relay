//! The bridge: calls come in as feeds of AMBE+2 frames, peers' slots
//! subscribe to talkgroups and groups of them, and every 60 ms each slot
//! that's playing sends its next burst.
//!
//! A slot plays one transmission at a time. It picks among the feeds on its
//! subscriptions whose voice is coming in now: an emergency first, then the
//! subscription listed first (within a group, its talkgroup listed first),
//! then the transmission that began first. When a transmission ends, the slot
//! holds that subscription for the hang time. Only an emergency takes a slot
//! from a transmission already playing. A change of talker ends the DMR
//! stream and starts another (a stream has one source ID).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::dmr::{lc_burst, silence_frame, talker_alias_lcs, voice_burst, voice_lc, Dmrd, Kind, DT_TERMINATOR_LC, DT_VOICE_LC_HEADER};

/// One burst every 60 ms (three 20 ms frames).
pub const TICK: Duration = Duration::from_millis(60);
/// The talkgroup that drops a slot's dynamic subscription.
pub const DISCONNECT_TG: u32 = 4000;
/// A live feed with nothing new for this long is over (its call end lost).
const LIVE_STALE: Duration = Duration::from_secs(10);
/// A stream starved this long is closed (a terminator); it reopens with a header.
const STARVE_CLOSE: u32 = 17; // ~1 s of ticks
/// A live feed is sending while its last frames came this recently; after
/// a longer gap its next frames start a new transmission.
const FRESH: Duration = Duration::from_millis(500);
/// A slot joining a transmission this young (frames) plays it from its start.
const JOIN_BACK: usize = 75;
/// The DMR talkgroup of the built-in group of every call.
pub const ALL_CALLS: u32 = 999_999;
const HEADERS: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct FeedKey {
    pub system: String,
    pub call: u32,
}

/// A transmission's talker, from frame `at` on.
#[derive(Clone, Debug, Serialize)]
pub struct Talker {
    pub at: usize,
    pub unit: u32,
    pub name: String,
}

pub struct Feed {
    pub key: FeedKey,
    pub dmr_tg: u32,
    pub tg: u32,
    pub tg_tag: String,
    pub emergency: bool,
    pub frames: Vec<[u8; 49]>,
    pub talkers: Vec<Talker>,
    /// No more frames will come.
    pub done: bool,
    /// Played as it's heard (else replayed after the call).
    pub live: bool,
    pub created: Instant,
    pub touched: Instant,
    /// Where the transmission coming in now began (frame), and when.
    pub seg_start: usize,
    pub seg_at: Instant,
    /// Slots that have taken it, and how far they played (frames).
    pub heard_by: BTreeMap<(u32, u8), usize>,
}

impl Feed {
    pub fn new(key: FeedKey, dmr_tg: u32, tg: u32, tg_tag: String, live: bool) -> Feed {
        let now = Instant::now();
        Feed {
            key,
            dmr_tg,
            tg,
            tg_tag,
            emergency: false,
            frames: Vec::new(),
            talkers: Vec::new(),
            done: false,
            live,
            created: now,
            touched: now,
            seg_start: 0,
            seg_at: now,
            heard_by: BTreeMap::new(),
        }
    }

    /// The talker at frame `i`.
    pub fn talker_at(&self, i: usize) -> Option<&Talker> {
        self.talkers.iter().rev().find(|t| t.at <= i).or(self.talkers.first())
    }

    /// Frames from a talker, appended.
    pub fn push(&mut self, unit: u32, name: &str, frames: impl IntoIterator<Item = [u8; 49]>) {
        if self.live && (self.frames.is_empty() || self.touched.elapsed() > FRESH) {
            (self.seg_start, self.seg_at) = (self.frames.len(), Instant::now());
        }
        if unit != 0 && self.talkers.last().is_none_or(|t| t.unit != unit) {
            self.talkers.push(Talker { at: self.frames.len(), unit, name: name.to_string() });
        } else if let Some(t) = self.talkers.last_mut().filter(|t| t.name.is_empty() && !name.is_empty()) {
            t.name = name.to_string();
        }
        self.frames.extend(frames);
        self.touched = Instant::now();
    }

    /// A live feed whose voice is coming in now.
    pub fn sending(&self) -> bool {
        self.live && self.touched.elapsed() <= FRESH
    }
}

/// A group of talkgroups a slot can subscribe to as one: by its own DMR
/// talkgroup ID, as if it were a talkgroup.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Group {
    pub id: u32,
    pub name: String,
    /// What it plays, first-listed first.
    pub members: Vec<Member>,
    /// Every call.
    pub all: bool,
    /// Calls go out on their own talkgroup IDs (else on the group's).
    pub keep_ids: bool,
    /// Made by the relay (not saved, not edited).
    pub builtin: bool,
}

/// Some of a system's talkgroups, as the recorder writes them: numbers,
/// ranges (`1000-1099`) and patterns (`507*`); none: all of them.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Member {
    /// Its system's short name (empty: every system).
    pub system: String,
    pub talkgroups: Vec<String>,
}

/// A group as the page makes it and the data folder keeps it ([`Group`]
/// without what the relay adds).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct GroupSetting {
    pub name: String,
    /// The DMR talkgroup a slot subscribes to it by.
    pub id: u32,
    /// What it plays, first-listed first.
    pub members: Vec<Member>,
    /// Each call goes out on its own talkgroup's ID, not the group's.
    pub keep_ids: bool,
}

impl GroupSetting {
    pub fn group(&self) -> Group {
        Group { id: self.id, name: self.name.clone(), members: self.members.clone(), keep_ids: self.keep_ids, ..Default::default() }
    }
}

impl Member {
    /// Where talkgroup `tg` of `system` ranks in it (None: not in it).
    fn rank(&self, system: &str, tg: u32) -> Option<usize> {
        if !self.system.is_empty() && self.system != system {
            return None;
        }
        if self.talkgroups.is_empty() {
            return Some(0);
        }
        self.talkgroups.iter().position(|e| trunk_recorder_plugin::filter::talkgroup_matches(e, tg))
    }

    /// Places it takes in a group's order.
    fn places(&self) -> usize {
        self.talkgroups.len().max(1)
    }
}

impl Group {
    /// Where a feed ranks in it (None: not in it).
    pub fn rank(&self, f: &Feed) -> Option<usize> {
        let mut at = 0;
        for m in &self.members {
            if let Some(i) = m.rank(&f.key.system, f.tg) {
                return Some(at + i);
            }
            at += m.places();
        }
        self.all.then_some(at)
    }

    /// Whether talkgroup `tg` of `system` is in it by name (not by a range,
    /// a pattern or a whole system), and so can be taken out alone.
    pub fn names(&self, system: &str, tg: u32) -> bool {
        let t = tg.to_string();
        self.members.iter().any(|m| m.system == system && m.talkgroups.iter().any(|e| e.trim() == t))
    }

    /// Talkgroup `tg` of `system` put in (`on`) or taken out by name.
    pub fn set(&mut self, system: &str, tg: u32, on: bool) {
        let t = tg.to_string();
        if on {
            if self.rank_of(system, tg).is_some() {
                return;
            }
            match self.members.iter_mut().find(|m| m.system == system && !m.talkgroups.is_empty()) {
                Some(m) => m.talkgroups.push(t),
                None => self.members.push(Member { system: system.to_string(), talkgroups: vec![t] }),
            }
        } else {
            // (An entry emptied goes: empty, it would be the whole system.)
            self.members.retain_mut(|m| {
                if m.system != system || !m.talkgroups.iter().any(|e| e.trim() == t) {
                    return true;
                }
                m.talkgroups.retain(|e| e.trim() != t);
                !m.talkgroups.is_empty()
            });
        }
    }

    fn rank_of(&self, system: &str, tg: u32) -> Option<usize> {
        self.members.iter().find_map(|m| m.rank(system, tg))
    }
}

/// A slot's stream in progress.
struct Play {
    feed: FeedKey,
    /// The subscription it plays for (a talkgroup or a group), and the
    /// talkgroup it goes out on.
    sub: u32,
    dst: u32,
    /// When its stream closed at a transmission's end (it may reopen for the next).
    closed_at: Option<Instant>,
    cursor: usize,
    /// The DMR stream is open (a header went out, no terminator yet).
    open: bool,
    stream: u32,
    seq: u8,
    headers: u8,
    /// Voice burst within the superframe (0 = A).
    n: usize,
    superframe: usize,
    lc: [u8; 9],
    alias: Vec<[u8; 9]>,
    unit: u32,
    starved: u32,
}

#[derive(Default)]
pub struct Slot {
    /// Chosen on the web page (kept across restarts).
    pub statics: Vec<u32>,
    /// Sent by the peer in its options (RPTO).
    pub options: Vec<u32>,
    /// Keyed up from the radio: the talkgroup and when it was last used.
    pub dynamic: Option<(u32, Instant)>,
    play: Option<Play>,
    hang: Option<(u32, Instant)>,
}

impl Slot {
    /// Its talkgroups, first-listed first.
    pub fn talkgroups(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.statics.iter().chain(&self.options).copied().collect();
        if let Some((tg, _)) = self.dynamic {
            v.push(tg);
        }
        let mut seen = BTreeSet::new();
        v.retain(|t| seen.insert(*t));
        v
    }
}

/// A peer, as far as the bridge cares.
#[derive(Clone, Debug)]
pub struct PeerSlots {
    pub id: u32,
    pub color_code: u8,
    pub slots: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Playing {
    pub peer: u32,
    pub slot: u8,
    pub dmr_tg: u32,
    /// The talkgroup it goes out on (a group's, or `dmr_tg`).
    pub sent_as: u32,
    pub tg_tag: String,
    pub unit: u32,
    pub unit_name: String,
    pub live: bool,
    /// Seconds behind the live edge (live) or into the call (replay).
    pub behind_s: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotView {
    pub slot: u8,
    pub statics: Vec<u32>,
    pub options: Vec<u32>,
    pub dynamic: Option<u32>,
    /// Seconds until the dynamic subscription lapses.
    pub dynamic_left_s: Option<u64>,
    pub playing: Option<Playing>,
}

pub struct Settings {
    pub hang: Duration,
    pub dynamic_timeout: Duration,
    /// How long a finished call may wait for a busy slot.
    pub replay_window: Duration,
    /// Frames buffered before a live feed starts playing.
    pub preroll: usize,
    /// Talker aliases (`{unit_name}`, `{unit}`, `{tg_tag}`, `{tg}`,
    /// `{system}`; `\n` breaks the line): on a slot subscribed to a group,
    /// and on one subscribed to the talkgroup itself (the group's when the
    /// talker has no name).
    pub alias_group: String,
    pub alias_talkgroup: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hang: Duration::from_secs(2),
            dynamic_timeout: Duration::from_secs(15 * 60),
            replay_window: Duration::from_secs(120),
            preroll: 15,
            alias_group: "{tg_tag}".into(),
            alias_talkgroup: "{unit_name}".into(),
        }
    }
}

#[derive(Default)]
pub struct Bridge {
    pub feeds: BTreeMap<FeedKey, Feed>,
    pub slots: BTreeMap<(u32, u8), Slot>,
    /// Groups by their DMR talkgroup.
    pub groups: BTreeMap<u32, Group>,
    pub settings: Settings,
    next_stream: u32,
    /// Each peer's last stream from its radio, per slot (a key-up is its first packet).
    keyed: BTreeMap<(u32, u8), u32>,
    /// What happened, newest last (for the page).
    pub log: VecDeque<(f64, String)>,
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

impl Bridge {
    pub fn new(settings: Settings) -> Bridge {
        Bridge { settings, next_stream: 0x1000_0000 | (now_s() as u32 & 0xFFFFFF), ..Default::default() }
    }

    pub fn note(&mut self, text: String) {
        self.log.push_back((now_s(), text));
        while self.log.len() > 100 {
            self.log.pop_front();
        }
    }

    /// Slots playing a call now.
    pub fn playing(&self) -> usize {
        self.slots.values().filter(|s| s.play.is_some()).count()
    }

    pub fn slot(&mut self, peer: u32, ts: u8) -> &mut Slot {
        self.slots.entry((peer, ts)).or_default()
    }

    /// A burst from a peer's radio. Its first packet subscribes the slot to
    /// its talkgroup (`known`: talkgroups on the roster); 4000 drops it.
    pub fn keyed(&mut self, peer: u32, d: &Dmrd, known: impl Fn(u32) -> bool) -> Option<String> {
        if d.private || self.keyed.get(&(peer, d.slot)) == Some(&d.stream) {
            return None;
        }
        self.keyed.insert((peer, d.slot), d.stream);
        let s = self.slot(peer, d.slot);
        let said = if d.dst == DISCONNECT_TG {
            s.dynamic.take().map(|(tg, _)| format!("{peer} TS{} dropped {tg}", d.slot))
        } else if known(d.dst) {
            s.dynamic = Some((d.dst, Instant::now()));
            Some(format!("{peer} TS{} keyed {} (from {})", d.slot, d.dst, d.src))
        } else {
            Some(format!("{peer} TS{} keyed {}: not on the roster", d.slot, d.dst))
        };
        if let Some(t) = &said {
            self.note(t.clone());
        }
        said
    }

    /// A peer went away: its slots stop (subscriptions chosen on the page stay).
    pub fn peer_gone(&mut self, peer: u32) {
        for ((p, _), s) in self.slots.iter_mut().filter(|((p, _), _)| *p == peer) {
            let _ = p;
            s.play = None;
            s.options.clear();
            s.dynamic = None;
        }
    }

    /// The talker alias for a stream: on a group, the talkgroup (many
    /// talkgroups share the group's ID, so that's the news); on a talkgroup
    /// itself, the talker, or the talkgroup when the talker has no name.
    fn alias(&self, f: &Feed, t: Option<&Talker>, system: &str, on_group: bool) -> String {
        let unit = t.map_or(0, |t| t.unit);
        let name = t.map(|t| t.name.as_str()).unwrap_or("");
        let tg_tag = if f.tg_tag.is_empty() { format!("{system} {}", f.tg) } else { f.tg_tag.clone() };
        let fill = |fmt: &str| {
            // `\n` / `\r` in a format: a line break for radios that show one.
            fmt.replace("\\n", "\n")
                .replace("\\r", "\r")
                .replace("{unit_name}", name)
                .replace("{unit}", &unit.to_string())
                .replace("{tg_tag}", &tg_tag)
                .replace("{tg}", &f.tg.to_string())
                .replace("{system}", system)
        };
        let s = &self.settings;
        if on_group || (name.is_empty() && s.alias_talkgroup.contains("{unit_name}")) {
            fill(&s.alias_group)
        } else {
            fill(&s.alias_talkgroup)
        }
    }

    /// Every slot's next burst. `peers`: who is logged in now.
    pub fn tick(&mut self, peers: &[PeerSlots]) -> Vec<(u32, Dmrd)> {
        let mut out = Vec::new();
        let now = Instant::now();
        for f in self.feeds.values_mut() {
            if f.live && !f.done && f.touched.elapsed() > LIVE_STALE {
                f.done = true;
            }
        }
        for p in peers {
            for &ts in &p.slots {
                if let Some(d) = self.slot_tick(p, ts, now) {
                    out.push((p.id, d));
                }
            }
        }
        self.prune();
        out
    }

    fn slot_tick(&mut self, p: &PeerSlots, ts: u8, now: Instant) -> Option<Dmrd> {
        let timeout = self.settings.dynamic_timeout;
        let s = self.slots.entry((p.id, ts)).or_default();
        if s.dynamic.is_some_and(|(_, t)| t.elapsed() > timeout) {
            s.dynamic = None;
        }
        let open = s.play.as_ref().is_some_and(|pl| pl.open);
        // An emergency on its subscriptions takes the slot from a routine call.
        let playing_emergency = open && s.play.as_ref().and_then(|pl| self.feeds.get(&pl.feed)).is_some_and(|f| f.emergency);
        if !playing_emergency {
            if let Some((k, sub, dst)) = self.pick(p.id, ts, open, now) {
                let old = self.slots.get_mut(&(p.id, ts)).unwrap().play.take();
                let pl = self.start(p.id, ts, k, sub, dst);
                self.slots.get_mut(&(p.id, ts)).unwrap().play = Some(pl);
                // A stream cut short gets its terminator first.
                if let Some(mut old) = old.filter(|o| o.open) {
                    let d = self.term(&mut old, p.color_code);
                    return Some(Dmrd { slot: ts, ..d });
                }
            }
        }
        self.advance(p, ts)
    }

    /// Whether a slot subscribed to `subs` takes a feed: (which subscription,
    /// rank within it, the talkgroup it goes out on).
    fn matches(&self, subs: &[u32], f: &Feed) -> Option<(usize, usize, u32)> {
        subs.iter().enumerate().find_map(|(i, &s)| {
            if s == f.dmr_tg {
                return Some((i, 0, s));
            }
            let g = self.groups.get(&s)?;
            g.rank(f).map(|r| (i, r, if g.keep_ids { f.dmr_tg } else { g.id }))
        })
    }

    /// The best transmission for a slot not playing one (`busy`: playing
    /// one, and only an emergency may have the slot). The feed, its
    /// subscription and the talkgroup it goes out on.
    fn pick(&self, peer: u32, ts: u8, busy: bool, now: Instant) -> Option<(FeedKey, u32, u32)> {
        let s = &self.slots[&(peer, ts)];
        let subs = s.talkgroups();
        if subs.is_empty() {
            return None;
        }
        let bound = s.play.as_ref().map(|pl| &pl.feed);
        let hang = s.hang.filter(|(_, until)| *until > now).map(|(tg, _)| tg);
        let window = self.settings.replay_window;
        let preroll = self.settings.preroll;
        self.feeds
            .values()
            .filter(|f| Some(&f.key) != bound)
            .filter(|f| {
                let played = f.heard_by.get(&(peer, ts));
                if f.live {
                    // Its transmission now: coming in, buffered, and not played here yet.
                    f.sending() && f.frames.len() - f.seg_start >= preroll && played.is_none_or(|&x| x <= f.seg_start)
                } else {
                    f.done && played.is_none() && f.created.elapsed() < window
                }
            })
            .filter_map(|f| self.matches(&subs, f).map(|m| (f, m)))
            .filter(|(f, (i, _, _))| if busy { f.emergency } else { f.emergency || hang.is_none_or(|h| h == subs[*i]) })
            .min_by_key(|(f, (i, r, _))| (!f.emergency, *i, *r, if f.live { f.seg_at } else { f.created }))
            .map(|(f, (i, _, dst))| (f.key.clone(), subs[i], dst))
    }

    fn start(&mut self, peer: u32, ts: u8, k: FeedKey, sub: u32, dst: u32) -> Play {
        let preroll = self.settings.preroll;
        let f = self.feeds.get_mut(&k).unwrap();
        // A live transmission from its start when it's young, else near its edge.
        let cursor = if !f.live {
            0
        } else if f.frames.len() - f.seg_start <= JOIN_BACK {
            f.seg_start
        } else {
            f.frames.len().saturating_sub(preroll)
        };
        f.heard_by.insert((peer, ts), cursor);
        let via = if dst != f.dmr_tg { format!(" as {dst}") } else { String::new() };
        let what = format!("TS{ts} → {peer}: {} {}{via}", f.dmr_tg, f.tg_tag);
        self.note(what);
        Play {
            feed: k,
            sub,
            dst,
            closed_at: None,
            cursor,
            open: false,
            stream: 0,
            seq: 0,
            headers: 0,
            n: 0,
            superframe: 0,
            lc: [0; 9],
            alias: Vec::new(),
            unit: 0,
            starved: 0,
        }
    }

    fn open(&mut self, pl: &mut Play) {
        let f = &self.feeds[&pl.feed];
        let t = f.talker_at(pl.cursor);
        pl.unit = t.map_or(0, |t| t.unit);
        pl.alias = talker_alias_lcs(&self.alias(f, t, &pl.feed.system, self.groups.contains_key(&pl.sub)));
        pl.lc = voice_lc(pl.dst, pl.unit, f.emergency);
        self.next_stream = self.next_stream.wrapping_add(1);
        pl.stream = self.next_stream;
        (pl.open, pl.seq, pl.headers, pl.n, pl.superframe, pl.starved, pl.closed_at) = (true, 0, HEADERS, 0, 0, 0, None);
    }

    fn advance(&mut self, p: &PeerSlots, ts: u8) -> Option<Dmrd> {
        let mut pl = self.slots.get_mut(&(p.id, ts))?.play.take()?;
        let cc = p.color_code;
        let r = self.step(&mut pl, cc);
        if let Some(f) = self.feeds.get_mut(&pl.feed) {
            f.heard_by.insert((p.id, ts), pl.cursor);
        }
        let hang = self.settings.hang;
        let slot = self.slots.get_mut(&(p.id, ts)).unwrap();
        match r {
            Step::Burst(d) => {
                if let Some((tg, at)) = slot.dynamic.as_mut().filter(|(tg, _)| *tg == pl.sub) {
                    let _ = tg;
                    *at = Instant::now();
                }
                slot.play = Some(pl);
                Some(Dmrd { slot: ts, ..d })
            }
            Step::Wait => {
                slot.play = Some(pl);
                None
            }
            Step::Closed(d) => {
                // The transmission's over: hold its subscription for the hang time.
                slot.hang = Some((pl.sub, Instant::now() + hang));
                pl.closed_at = Some(Instant::now());
                slot.play = Some(pl);
                Some(Dmrd { slot: ts, ..d })
            }
            Step::End(d) => {
                slot.hang = Some((pl.sub, Instant::now() + hang));
                d.map(|d| Dmrd { slot: ts, ..d })
            }
        }
    }

    fn step(&mut self, pl: &mut Play, cc: u8) -> Step {
        let Some(f) = self.feeds.get(&pl.feed) else {
            return Step::End(pl.open.then(|| self.term(pl, cc)));
        };
        let avail = f.frames.len().saturating_sub(pl.cursor);
        let (done, sending) = (f.done, f.sending());
        if !pl.open {
            if avail == 0 {
                return if done { Step::End(None) } else { Step::Wait };
            }
            // The next transmission of a live call: once it's buffered.
            if f.live && sending && avail < self.settings.preroll {
                return Step::Wait;
            }
            self.open(pl);
        }
        let f = &self.feeds[&pl.feed];
        if pl.headers > 0 {
            pl.headers -= 1;
            let d = lc_burst(cc, DT_VOICE_LC_HEADER, &pl.lc);
            return Step::Burst(self.packet(pl, Kind::Data(DT_VOICE_LC_HEADER), d));
        }
        // Another talker from here on: close this stream, open the next.
        if avail > 0 && f.talker_at(pl.cursor).is_some_and(|t| t.unit != pl.unit) {
            pl.open = false;
            return Step::Burst(self.term(pl, cc));
        }
        if avail < 3 {
            if sending && !done {
                // Late frames: hold the stream open with silence.
                pl.starved += 1;
                if pl.starved >= STARVE_CLOSE {
                    pl.open = false;
                    return Step::Closed(self.term(pl, cc));
                }
                let d = self.voice(pl, cc, [silence_frame(); 3]);
                return Step::Burst(d);
            }
            if avail == 0 {
                // The transmission's over (or the call).
                pl.open = false;
                let d = self.term(pl, cc);
                return if done { Step::End(Some(d)) } else { Step::Closed(d) };
            }
            // Its last frames, filled out with silence.
        }
        pl.starved = 0;
        let frames: [[u8; 49]; 3] = std::array::from_fn(|k| f.frames.get(pl.cursor + k).copied().unwrap_or_else(silence_frame));
        pl.cursor = (pl.cursor + 3).min(f.frames.len());
        Step::Burst(self.voice(pl, cc, frames))
    }

    fn voice(&self, pl: &mut Play, cc: u8, frames: [[u8; 49]; 3]) -> Dmrd {
        // The superframe's embedded LC: the alias's parts first (a short
        // transmission still gets its name across), then the voice LC, in turn.
        let k = pl.superframe % (1 + pl.alias.len());
        let emb = pl.alias.get(k).copied().unwrap_or(pl.lc);
        let n = pl.n;
        let d = voice_burst(cc, n, &frames, Some(&emb));
        pl.n = (n + 1) % 6;
        if pl.n == 0 {
            pl.superframe += 1;
        }
        self.packet(pl, if n == 0 { Kind::VoiceSync } else { Kind::Voice(n as u8) }, d)
    }

    fn term(&self, pl: &mut Play, cc: u8) -> Dmrd {
        let d = lc_burst(cc, DT_TERMINATOR_LC, &pl.lc);
        self.packet(pl, Kind::Data(DT_TERMINATOR_LC), d)
    }

    fn packet(&self, pl: &mut Play, kind: Kind, data: [u8; 33]) -> Dmrd {
        let d = Dmrd {
            seq: pl.seq,
            src: pl.unit,
            dst: u32::from_be_bytes([0, pl.lc[3], pl.lc[4], pl.lc[5]]),
            peer: 0,
            slot: 0,
            private: false,
            kind,
            stream: pl.stream,
            data,
        };
        pl.seq = pl.seq.wrapping_add(1);
        d
    }

    /// Feeds no slot will play any more.
    fn prune(&mut self) {
        let playing: BTreeSet<FeedKey> = self.slots.values().filter_map(|s| s.play.as_ref().map(|p| p.feed.clone())).collect();
        let window = self.settings.replay_window;
        self.feeds.retain(|k, f| playing.contains(k) || !f.done || f.touched.elapsed() < window.max(Duration::from_secs(5)));
        while self.feeds.len() > 300 {
            let Some(oldest) = self.feeds.values().filter(|f| !playing.contains(&f.key)).min_by_key(|f| f.created).map(|f| f.key.clone()) else { break };
            self.feeds.remove(&oldest);
        }
    }

    pub fn view(&self, peer: u32, slots: &[u8]) -> Vec<SlotView> {
        slots
            .iter()
            .map(|&ts| {
                let s = self.slots.get(&(peer, ts));
                let playing = s.and_then(|s| s.play.as_ref()).filter(|pl| pl.open).and_then(|pl| {
                    let f = self.feeds.get(&pl.feed)?;
                    let t = f.talker_at(pl.cursor.saturating_sub(1));
                    let behind = if f.live { f.frames.len().saturating_sub(pl.cursor) } else { pl.cursor };
                    Some(Playing {
                        peer,
                        slot: ts,
                        dmr_tg: f.dmr_tg,
                        sent_as: pl.dst,
                        tg_tag: f.tg_tag.clone(),
                        unit: t.map_or(0, |t| t.unit),
                        unit_name: t.map(|t| t.name.clone()).unwrap_or_default(),
                        live: f.live,
                        behind_s: behind as f64 * 0.02,
                    })
                });
                SlotView {
                    slot: ts,
                    statics: s.map(|s| s.statics.clone()).unwrap_or_default(),
                    options: s.map(|s| s.options.clone()).unwrap_or_default(),
                    dynamic: s.and_then(|s| s.dynamic.map(|d| d.0)),
                    dynamic_left_s: s.and_then(|s| s.dynamic).map(|(_, at)| self.settings.dynamic_timeout.saturating_sub(at.elapsed()).as_secs()),
                    playing,
                }
            })
            .collect()
    }
}

enum Step {
    Burst(Dmrd),
    /// Nothing to send this tick (a stream not yet open).
    Wait,
    /// A transmission's over: its terminator. The slot stays with the call
    /// (its next transmission may come), holding the subscription for the hang time.
    Closed(Dmrd),
    /// Done: the last packet, if one is owed.
    End(Option<Dmrd>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(call: u32) -> FeedKey {
        FeedKey { system: "s".into(), call }
    }

    fn feed(b: &mut Bridge, call: u32, tg: u32, frames: usize, live: bool) {
        let mut f = Feed::new(key(call), tg, tg, format!("TG {tg}"), live);
        f.push(1000 + call, "Unit", (0..frames).map(|i| [((i + call as usize) & 1) as u8; 49]));
        f.done = !live;
        b.feeds.insert(key(call), f);
    }

    const P: PeerSlots = PeerSlots { id: 7, color_code: 1, slots: Vec::new() };

    fn peer() -> Vec<PeerSlots> {
        vec![PeerSlots { slots: vec![1, 2], ..P }]
    }

    #[test]
    fn a_call_plays_header_voice_terminator() {
        let mut b = Bridge::new(Settings::default());
        b.slot(7, 2).statics = vec![100];
        feed(&mut b, 1, 100, 36, false);
        let mut kinds = Vec::new();
        for _ in 0..20 {
            for (_, d) in b.tick(&peer()) {
                assert_eq!((d.slot, d.dst, d.src), (2, 100, 1001));
                kinds.push(d.kind);
            }
        }
        use Kind::*;
        assert_eq!(
            kinds,
            [
                Data(1),
                Data(1),
                VoiceSync,
                Voice(1),
                Voice(2),
                Voice(3),
                Voice(4),
                Voice(5),
                VoiceSync,
                Voice(1),
                Voice(2),
                Voice(3),
                Voice(4),
                Voice(5),
                Data(2)
            ]
        );
        // Not again.
        assert!(b.tick(&peer()).is_empty());
    }

    #[test]
    fn talkers_get_their_own_streams() {
        let mut b = Bridge::new(Settings::default());
        b.slot(7, 1).statics = vec![100];
        let mut f = Feed::new(key(1), 100, 100, String::new(), false);
        f.push(11, "A", [[0; 49]; 6]);
        f.push(22, "B", [[0; 49]; 6]);
        f.done = true;
        b.feeds.insert(key(1), f);
        let mut got = Vec::new();
        for _ in 0..20 {
            got.extend(b.tick(&peer()).into_iter().map(|(_, d)| (d.src, d.stream, d.kind)));
        }
        let streams: BTreeSet<u32> = got.iter().map(|g| g.1).collect();
        assert_eq!(streams.len(), 2);
        assert_eq!(got.iter().filter(|g| g.2 == Kind::Data(2)).count(), 2);
        assert!(got.iter().take_while(|g| g.2 != Kind::Data(2)).all(|g| g.0 == 11));
        assert_eq!(got.last().unwrap().0, 22);
    }

    #[test]
    fn first_listed_talkgroup_wins_and_emergencies_take_over() {
        let mut b = Bridge::new(Settings::default());
        b.slot(7, 1).statics = vec![200, 100];
        feed(&mut b, 1, 100, 300, false);
        feed(&mut b, 2, 200, 300, false);
        let first = b.tick(&peer());
        assert_eq!(first[0].1.dst, 200);
        for _ in 0..5 {
            b.tick(&peer());
        }
        feed(&mut b, 3, 100, 30, false);
        b.feeds.get_mut(&key(3)).unwrap().emergency = true;
        let d = b.tick(&peer());
        assert_eq!(d[0].1.kind, Kind::Data(2), "the routine call is closed");
        let d = b.tick(&peer());
        assert_eq!((d[0].1.dst, d[0].1.kind), (100, Kind::Data(1)));
    }

    #[test]
    fn a_live_call_waits_for_its_preroll_and_bridges_gaps() {
        let mut b = Bridge::new(Settings::default());
        b.slot(7, 1).statics = vec![100];
        feed(&mut b, 1, 100, 6, true);
        assert!(b.tick(&peer()).is_empty(), "not enough buffered");
        b.feeds.get_mut(&key(1)).unwrap().push(1001, "Unit", [[0; 49]; 12]);
        let mut n = 0;
        for _ in 0..12 {
            n += b.tick(&peer()).len();
        }
        // Two headers, five voice bursts (it joined 15 frames from the edge), then silence holds it open.
        assert_eq!(n, 12);
        assert!(b.slots[&(7, 1)].play.as_ref().unwrap().starved > 0);
        b.feeds.get_mut(&key(1)).unwrap().done = true;
        let last: Vec<Kind> = (0..3).flat_map(|_| b.tick(&peer())).map(|(_, d)| d.kind).collect();
        assert_eq!(last, [Kind::Data(2)]);
    }

    #[test]
    fn keying_subscribes_and_4000_drops() {
        let mut b = Bridge::new(Settings::default());
        let d = Dmrd { seq: 0, src: 3112345, dst: 100, peer: 7, slot: 2, private: false, kind: Kind::Data(1), stream: 5, data: [0; 33] };
        b.keyed(7, &d, |t| t == 100);
        assert_eq!(b.slots[&(7, 2)].dynamic.map(|d| d.0), Some(100));
        // The rest of that transmission changes nothing.
        b.keyed(7, &Dmrd { dst: DISCONNECT_TG, kind: Kind::Voice(1), ..d.clone() }, |_| true);
        assert!(b.slots[&(7, 2)].dynamic.is_some());
        b.keyed(7, &Dmrd { dst: DISCONNECT_TG, stream: 6, ..d }, |_| true);
        assert!(b.slots[&(7, 2)].dynamic.is_none());
    }

    fn ago(ms: u64) -> Instant {
        Instant::now() - Duration::from_millis(ms)
    }

    #[test]
    fn a_transmission_that_ends_closes_at_once_and_isnt_joined_late() {
        let mut b = Bridge::new(Settings::default());
        b.slot(7, 1).statics = vec![100, 200];
        // Talkgroup 100's transmission ended a second ago; 200's is coming in now.
        feed(&mut b, 1, 100, 300, true);
        b.feeds.get_mut(&key(1)).unwrap().touched = ago(1000);
        feed(&mut b, 2, 200, 30, true);
        let first = b.tick(&peer());
        assert_eq!((first[0].1.dst, first[0].1.kind), (200, Kind::Data(1)), "the one sending now, though listed second");
        let mut kinds = Vec::new();
        for _ in 0..11 {
            kinds.extend(b.tick(&peer()).into_iter().map(|(_, d)| d.kind));
        }
        // The second header, ten voice bursts (all 30 frames, from the start),
        // then its terminator as soon as it ran dry: no silence after.
        b.feeds.get_mut(&key(2)).unwrap().touched = ago(600);
        kinds.extend(b.tick(&peer()).into_iter().map(|(_, d)| d.kind));
        assert_eq!(kinds.iter().filter(|k| matches!(k, Kind::VoiceSync | Kind::Voice(_))).count(), 10);
        assert_eq!(kinds.last(), Some(&Kind::Data(2)));
    }

    #[test]
    fn a_slot_holds_its_talkgroup_for_the_hang_time_then_moves_on() {
        let mut b = Bridge::new(Settings { hang: Duration::from_millis(200), ..Settings::default() });
        b.slot(7, 1).statics = vec![100, 200];
        feed(&mut b, 1, 100, 15, true);
        for _ in 0..8 {
            b.tick(&peer());
        }
        b.feeds.get_mut(&key(1)).unwrap().touched = ago(600);
        let d = b.tick(&peer());
        assert_eq!(d[0].1.kind, Kind::Data(2));
        // Another talkgroup starts within the hang time: not yet.
        feed(&mut b, 2, 200, 30, true);
        assert!(b.tick(&peer()).is_empty());
        // The same talkgroup's next transmission (a new call) would be taken now…
        std::thread::sleep(Duration::from_millis(250));
        // …and after the hang time the other one is.
        let d = b.tick(&peer());
        assert_eq!((d[0].1.dst, d[0].1.kind), (200, Kind::Data(1)));
    }

    #[test]
    fn groups_play_their_talkgroups_as_one() {
        let mut b = Bridge::new(Settings::default());
        b.groups.insert(ALL_CALLS, Group { id: ALL_CALLS, name: "All".into(), all: true, ..Default::default() });
        b.groups.insert(
            900,
            Group {
                id: 900,
                name: "Fire".into(),
                members: vec![Member { system: "s".into(), talkgroups: vec!["300".into(), "200".into()] }],
                keep_ids: true,
                ..Default::default()
            },
        );
        b.slot(7, 1).statics = vec![ALL_CALLS];
        b.slot(7, 2).statics = vec![900];
        feed(&mut b, 1, 100, 30, true);
        feed(&mut b, 2, 200, 30, true);
        feed(&mut b, 3, 300, 30, true);
        let out = b.tick(&peer());
        let ts1 = out.iter().find(|(_, d)| d.slot == 1).unwrap();
        let ts2 = out.iter().find(|(_, d)| d.slot == 2).unwrap();
        assert_eq!(ts1.1.dst, ALL_CALLS, "on the group's own talkgroup");
        assert_eq!(ts2.1.dst, 300, "the group's first-listed member, on its own talkgroup");
    }

    #[test]
    fn the_talker_alias_goes_first() {
        let mut b = Bridge::new(Settings::default());
        b.slot(7, 1).statics = vec![100];
        feed(&mut b, 1, 100, 60, false);
        let mut lcs = Vec::new();
        for _ in 0..12 {
            for (_, d) in b.tick(&peer()) {
                if d.kind == Kind::Voice(4) {
                    let bits = trunk_core::dmr::fec::unpack(&d.data);
                    lcs.push(bits[116..148].to_vec());
                }
            }
        }
        // Superframe 0 carries the alias header: not the voice LC.
        let first: Vec<u8> = lcs[0].clone();
        let lc_sf = crate::dmr::voice_burst(1, 4, &[[0; 49]; 3], Some(&voice_lc(100, 1001, false)));
        assert_ne!(first, trunk_core::dmr::fec::unpack(&lc_sf)[116..148].to_vec());
    }

    #[test]
    fn groups_name_the_talkgroup_and_talkgroups_the_talker() {
        let mut b = Bridge::new(Settings { alias_talkgroup: "{unit_name}\\n{tg_tag}".into(), ..Settings::default() });
        b.groups.insert(9, Group { id: 9, all: true, ..Default::default() });
        let f = Feed::new(key(1), 100, 100, "Westport FD Disp".into(), true);
        let named = Talker { at: 0, unit: 1234, name: "Eng 4".into() };
        let unnamed = Talker { name: String::new(), ..named.clone() };
        assert_eq!(b.alias(&f, Some(&named), "s", true), "Westport FD Disp");
        assert_eq!(b.alias(&f, Some(&named), "s", false), "Eng 4\nWestport FD Disp");
        assert_eq!(b.alias(&f, Some(&unnamed), "s", false), "Westport FD Disp");
        assert_eq!(b.alias(&Feed::new(key(2), 100, 100, String::new(), true), None, "CLMRN", false), "CLMRN 100");
    }
    #[test]
    fn group_members_match_as_the_recorder_writes_talkgroups() {
        let g = Group {
            id: 900,
            members: vec![
                Member { system: "a".into(), talkgroups: vec!["8703".into(), "8800-8899".into()] },
                Member { system: "b".into(), talkgroups: vec![] },
            ],
            ..Default::default()
        };
        let f = |sys: &str, tg: u32| Feed::new(FeedKey { system: sys.into(), call: 1 }, tg, tg, String::new(), true);
        assert_eq!(g.rank(&f("a", 8703)), Some(0));
        assert_eq!(g.rank(&f("a", 8850)), Some(1), "in a range");
        assert_eq!(g.rank(&f("b", 5)), Some(2), "a whole system, after the entries before it");
        assert_eq!(g.rank(&f("a", 9000)), None);
        assert!(g.names("a", 8703) && !g.names("a", 8850) && !g.names("b", 5));
        let mut h = g.clone();
        h.set("a", 8703, false);
        assert_eq!(h.members[0].talkgroups, vec!["8800-8899".to_string()]);
        h.set("a", 8800 + 1, true);
        assert_eq!(h.members[0].talkgroups.len(), 1, "already in by its range");
        h.set("c", 7, true);
        h.set("c", 7, false);
        assert!(h.members.iter().all(|m| m.system != "c"), "an entry emptied goes, not left as the whole system");
    }
}
