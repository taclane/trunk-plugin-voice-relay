//! The relay's page: peers and their slots, the calls going on, and the
//! roster of talkgroups to put on a slot. Its JSON API:
//!
//! ```text
//! GET  /api/state                   everything the page shows
//! POST /api/subs {peer,slot,dmr,on} add / remove a talkgroup on a peer's slot
//! POST /api/dynamic {peer,slot}     drop the slot's keyed-up talkgroup
//! POST /api/groups {id,name,keepIds}         make a group, or rename it
//! POST /api/groups/delete {id}                remove a group
//! POST /api/groups/member {id,system,tg,on}   put a talkgroup in a group, or take it out
//! POST /api/systems {system,skip,tgBase}      a system relayed or not, and its base (restarts the plugin)
//! GET  /contacts.csv[?prefix=9]     groups, talkgroups and named radios, for a codeplug
//! ```

use std::io::Read;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::Shared;

const PAGE: &str = include_str!("page.html");

pub fn spawn(shared: Arc<Shared>, server: Server) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            handle(&shared, req);
        }
    })
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn reply(req: Request, status: u16, kind: &str, body: String) {
    let r = Response::from_string(body).with_status_code(status).with_header(header("Content-Type", kind)).with_header(header("Cache-Control", "no-store"));
    let _ = req.respond(r);
}

#[derive(Deserialize)]
struct Subs {
    peer: u32,
    slot: u8,
    #[serde(default)]
    dmr: u32,
    #[serde(default)]
    on: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupEdit {
    id: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    keep_ids: Option<bool>,
    #[serde(default)]
    system: String,
    #[serde(default)]
    tg: u32,
    #[serde(default)]
    on: bool,
}

fn query<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    url.split_once('?')?.1.split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

fn handle(shared: &Shared, mut req: Request) {
    let full = req.url().to_string();
    let url = full.split('?').next().unwrap_or("/").to_string();
    match (req.method(), url.as_str()) {
        (Method::Get, "/") => reply(req, 200, "text/html; charset=utf-8", PAGE.to_string()),
        (Method::Get, "/api/state") => reply(req, 200, "application/json", state(shared).to_string()),
        (Method::Get, "/contacts.csv") => {
            // Only digits: what a hotspot puts before a network talkgroup on RF.
            let prefix: String = match query(&full, "prefix") {
                Some(p) => p.chars().filter(char::is_ascii_digit).take(2).collect(),
                None => shared.rf_prefix.lock().unwrap().clone(),
            };
            let groups: Vec<(u32, String)> = shared.bridge.lock().unwrap().groups.values().map(|g| (g.id, g.name.clone())).collect();
            let csv = shared.roster.lock().unwrap().contacts_csv(&groups, &prefix);
            let r = Response::from_string(csv)
                .with_header(header("Content-Type", "text/csv"))
                .with_header(header("Content-Disposition", "attachment; filename=\"voice-relay-contacts.csv\""));
            let _ = req.respond(r);
        }
        (Method::Post, "/api/groups" | "/api/groups/delete" | "/api/groups/member") => {
            let mut body = String::new();
            let _ = req.as_reader().take(65536).read_to_string(&mut body);
            let Ok(e) = serde_json::from_str::<GroupEdit>(&body) else { return reply(req, 400, "text/plain", "bad request".into()) };
            let mut groups = shared.groups.lock().unwrap().clone();
            let at = groups.iter().position(|g| g.id == e.id);
            let said = match (url.as_str(), at) {
                ("/api/groups/delete", Some(i)) => format!("group {} {} removed", e.id, groups.remove(i).name),
                ("/api/groups/member", Some(i)) => {
                    let mut g = groups[i].group();
                    if !e.on && !g.names(&e.system, e.tg) {
                        return reply(req, 400, "text/plain", "it's in the group by a range or a whole system, not by its own number".into());
                    }
                    g.set(&e.system, e.tg, e.on);
                    groups[i].members = g.members;
                    format!("group {}: {} {} {}", groups[i].name, e.system, e.tg, if e.on { "added" } else { "taken out" })
                }
                ("/api/groups", Some(i)) => {
                    if let Some(n) = &e.name {
                        groups[i].name = n.trim().chars().take(32).collect();
                    }
                    if let Some(k) = e.keep_ids {
                        groups[i].keep_ids = k;
                    }
                    format!("group {} {} changed", e.id, groups[i].name)
                }
                ("/api/groups", None) => {
                    let name: String = e.name.as_deref().unwrap_or("").trim().chars().take(32).collect();
                    let name = if name.is_empty() { format!("Group {}", e.id) } else { name };
                    groups.push(crate::bridge::GroupSetting { id: e.id, name: name.clone(), keep_ids: e.keep_ids.unwrap_or(false), members: Vec::new() });
                    format!("group {} {name} made", e.id)
                }
                _ => return reply(req, 400, "text/plain", "no such group (a built-in one can't be changed)".into()),
            };
            if let Err(e) = shared.edit_groups(groups) {
                return reply(req, 400, "text/plain", e);
            }
            shared.bridge.lock().unwrap().note(said);
            reply(req, 200, "application/json", state(shared).to_string())
        }
        (Method::Post, "/api/systems") => {
            let mut body = String::new();
            let _ = req.as_reader().take(4096).read_to_string(&mut body);
            let Ok(s) = serde_json::from_str::<crate::SystemSetting>(&body) else { return reply(req, 400, "text/plain", "bad request".into()) };
            if !shared.all_systems.iter().any(|(n, _)| *n == s.system) {
                return reply(req, 400, "text/plain", format!("the recorder has no system {}", s.system));
            }
            if let Some(b) = s.tg_base {
                if b == 0 || b > 0xFF_FFFF - 65_535 {
                    return reply(req, 400, "text/plain", "a base is 1 to 16711680, so its talkgroups fit in a DMR ID".into());
                }
                let taken = { shared.roster.lock().unwrap().bases.iter().any(|(n, x)| *n != s.system && *x == b) };
                if taken {
                    return reply(req, 400, "text/plain", format!("another system's base is {b}"));
                }
            }
            shared.bridge.lock().unwrap().note(format!(
                "{}: {}; restarting",
                s.system,
                if s.skip { "left out".to_string() } else { s.tg_base.map_or("base by its place".into(), |b| format!("base {b}")) }
            ));
            shared.set_system(s);
            reply(req, 200, "application/json", state(shared).to_string())
        }
        (Method::Post, "/api/subs" | "/api/dynamic") => {
            let mut body = String::new();
            let _ = req.as_reader().take(4096).read_to_string(&mut body);
            let Ok(s) = serde_json::from_str::<Subs>(&body) else { return reply(req, 400, "text/plain", "bad request".into()) };
            if !(1..=2).contains(&s.slot) {
                return reply(req, 400, "text/plain", "slot is 1 or 2".into());
            }
            if url == "/api/dynamic" {
                let mut b = shared.bridge.lock().unwrap();
                b.slot(s.peer, s.slot).dynamic = None;
                b.note(format!("{} TS{}: keyed talkgroup dropped on the page", s.peer, s.slot));
            } else {
                {
                    let mut subs = shared.subs.lock().unwrap();
                    let list = &mut subs.entry(s.peer).or_default()[s.slot as usize - 1];
                    list.retain(|t| *t != s.dmr);
                    if s.on {
                        list.push(s.dmr);
                    }
                }
                shared.save_subs();
                shared.apply_subs(s.peer);
                shared.bridge.lock().unwrap().note(format!("{} TS{}: {} {} on the page", s.peer, s.slot, s.dmr, if s.on { "added" } else { "removed" }));
            }
            reply(req, 200, "application/json", state(shared).to_string())
        }
        _ => reply(req, 404, "text/plain", "not found".into()),
    }
}

fn state(shared: &Shared) -> Value {
    let peers: Vec<(u32, Value, Vec<u8>)> = {
        let m = shared.master.lock().unwrap();
        m.peers
            .values()
            .filter(|p| p.login == crate::mmdvm::Login::Running)
            .map(|p| {
                let mut v = serde_json::to_value(&p.config).unwrap_or(json!({}));
                v["id"] = json!(p.id);
                v["addr"] = json!(p.addr.to_string());
                v["since"] = json!(p.since);
                v["options"] = json!(p.options);
                v["online"] = json!(true);
                v["seenAgo"] = json!(p.seen.elapsed().as_secs());
                (p.id, v, p.config.time_slots())
            })
            .collect()
    };
    let saved: Vec<u32> = shared.subs.lock().unwrap().keys().copied().collect();
    let b = shared.bridge.lock().unwrap();
    let mut out_peers = Vec::new();
    let mut on_air: Vec<Value> = Vec::new();
    for (id, mut v, slots) in peers {
        let views = b.view(id, &slots);
        for s in views.iter().filter_map(|s| s.playing.as_ref()) {
            on_air.push(serde_json::to_value(s).unwrap());
        }
        v["slots"] = serde_json::to_value(views).unwrap();
        out_peers.push(v);
    }
    // Peers with talkgroups chosen but not logged in now.
    for id in saved {
        if !out_peers.iter().any(|p| p["id"] == json!(id)) {
            out_peers.push(json!({ "id": id, "online": false, "callsign": "", "slots": b.view(id, &[1, 2]) }));
        }
    }
    let log: Vec<Value> = b.log.iter().rev().take(40).map(|(t, s)| json!([t, s])).collect();
    let groups: Vec<&crate::bridge::Group> = b.groups.values().collect();
    let groups = serde_json::to_value(groups).unwrap_or(json!([]));
    let feeds: Vec<Value> =
        b.feeds.values().map(|f| json!({ "system": f.key.system, "call": f.key.call, "frames": f.frames.len(), "done": f.done, "live": f.live })).collect();
    drop(b);
    let r = shared.roster.lock().unwrap();
    let chosen = shared.systems.lock().unwrap().clone();
    let systems: Vec<Value> = shared
        .all_systems
        .iter()
        .enumerate()
        .map(|(i, (name, kind))| {
            let c = chosen.iter().find(|c| &c.system == name);
            json!({ "system": name, "kind": kind, "skip": c.is_some_and(|c| c.skip), "tgBase": c.and_then(|c| c.tg_base), "byPlace": (i as u32 + 1) * 100_000, "base": r.bases.get(name) })
        })
        .collect();
    let mut tgs: Vec<&crate::roster::TgEntry> = r.tgs.values().collect();
    tgs.sort_by(|a, b| b.last.total_cmp(&a.last));
    json!({
        "mode": if shared.live.load(std::sync::atomic::Ordering::Relaxed) { "live" } else { "after each call" },
        "master": shared.master_addr,
        "peers": out_peers,
        "onAir": on_air,
        "active": r.active.values().collect::<Vec<_>>(),
        "recent": r.recent.iter().take(25).collect::<Vec<_>>(),
        "talkgroups": tgs,
        "groups": groups,
        "rfPrefix": *shared.rf_prefix.lock().unwrap(),
        "systems": systems,
        "units": r.units.len(),
        "feeds": feeds,
        "log": log,
    })
}
