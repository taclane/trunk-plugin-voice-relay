# Voice Relay

A Trunk Recorder Pro plugin that relays live calls to digital radios. It is an
MMDVM-compatible network master, and it carries calls as DMR today: MMDVM hotspots and repeaters log into it as
they would into any DMR network, and each slot plays the talkgroups it
subscribes to.

Nothing goes the other way. A radio keying up only tells the bridge which
talkgroup its slot should play.

The recorder gives it vocoder frames, not audio, so other digital modes a
hotspot speaks (P25, NXDN) could be added as outputs on the same frames.

## Install

In Trunk Recorder Pro: **Plugins → Install from GitHub…** with
`https://github.com/taclane/trunk-plugin-voice-relay`, or

```sh
trunk-pro plugin install https://github.com/taclane/trunk-plugin-voice-relay
```

Calls play live with a recorder that has the plugin `voice` topic (SDK 0.11.0, after
Trunk Recorder Pro 0.3.6). With an older one, each call plays once it's done, from its
`.sdr` frame file (see [Live or after the call](#live-or-after-the-call)).

To build it yourself: `cargo build --release`, then add `target/release/voice-relay` with
**Add from a file…** on the Plugins page.

## Voice

| Heard as | On DMR |
|---|---|
| P25 Phase 2, DMR, NXDN (AMBE+2) | bit for bit, with fresh FEC |
| P25 Phase 1, ProVoice (IMBE) | converted to AMBE+2 by its model parameters (no PCM in between) |
| Analog | not carried |

The conversion dequantizes each IMBE frame to its pitch, voicing and spectral
amplitudes, and quantizes them against the half-rate decoder's own
tables. An AMBE+2 decoder runs in the loop, so the next frame is
predicted the way a radio predicts it.

`cargo run --release --example abtest -- call.sdr out` writes a Phase 1
call two ways: IMBE as TRP plays it, and converted to AMBE+2. It also prints
how far apart the two spectra are.

## Talkgroups

Every talkgroup heard gets a DMR talkgroup ID: its system's base plus its
number. The base is 100000 × the system's place in the list unless a system
sets its own. So CLMRN 8703 is 108703 when CLMRN is first.

A slot plays the talkgroups chosen for it in any of three ways:

- **On the page:** kept across restarts.
- **In the hotspot's options:** `TS1=108703,108704;TS2=226121`.
- **Keyed up from a radio:** a short PTT on the talkgroup. It lasts 15 minutes
  without a call, and TG 4000 drops it.

**Groups** play many talkgroups on one slot, as one talkgroup ID:

- **All calls** (999999) and **each system's whole traffic** (its base: 100000, 200000, …) are built in.
- Make your own on the relay page: name it, give it an ID, and pick its talkgroups from the
  roster. They play first-listed first, out on the group's ID (one RX entry on the radio), or
  on their own IDs if it keeps them. Groups are kept in the plugin's data folder
  (`groups.json`); a system's talkgroups can be given there as ranges or patterns too.

A slot plays one transmission at a time, joining only voice that's coming in now. An emergency comes first, then the talkgroup
listed first, then the call that started first. After a call, the slot holds
its talkgroup for the hang time. Only an emergency takes a slot away from a
call already playing. Each talker gets a stream of their own, with their name
as the DMR talker alias. **Contacts CSV** on the page lists the talkgroups and
named radios, for a codeplug.

## Live or after the call

With a recorder that has the `voice` topic, calls play live, with about 300 ms
of buffer. With one that doesn't, each call plays once it's done, from its
`.sdr` frame file (the recorder keeps frames for plugins that take
concluded calls).

## Hooking up a hotspot

Give the hotspot a DMR network with this bridge as the master: the address,
port 62031 and the bridge's password.

- **A talkgroup prefix:** some hotspots put a digit in front of a custom network's
  talkgroups on RF (with a **9**, 108703 is heard as 9108703). Set **RF prefix** to that
  digit in the settings (or on the relay page) before downloading the contacts, and key
  talkgroups with it.
- **Talker alias** needs the hotspot's "embedded LC only" option off. On a group
  it names the talkgroup (many share the group's ID); on a talkgroup itself, the talker,
  or the talkgroup when the talker has no name. Both are settings; `\n` breaks the line.

The plugin's settings (ports, password, aliases, RF prefix) are on its own page in the
recorder, which links to the relay page (port 8096). The relay page manages the rest: peers
and what each slot plays, groups, which systems are relayed and their talkgroup bases, the
calls on the air, and a **Connect a hotspot** panel. Changing the ports or how calls play,
or a system on the relay page, restarts the plugin; everything else applies at once.

## Testing without a hotspot

`cargo run --example peer -- 127.0.0.1:62031 3112345 <password> "TS2=100251" out.wav 60`
logs in as a hotspot and prints each stream's LC and talker alias. It decodes
the voice to a WAV with TRP's AMBE+2 decoder.
