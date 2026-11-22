//! Sonos: SSDP discovery + UPnP/SOAP control, and the engine that keeps the
//! selected speakers playing our stream.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::log;
use crate::net::{http_get, soap_call};
use crate::state::Shared;
use crate::util::{contains_ci, Res};
use crate::xml;

pub const SSDP_ADDR: &str = "239.255.255.250:1900";
pub const TARGET: &str = "urn:schemas-upnp-org:device:ZonePlayer:1";

const AV: &str = "/MediaRenderer/AVTransport/Control";
const AV_SVC: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RC: &str = "/MediaRenderer/RenderingControl/Control";
const RC_SVC: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const ZGT: &str = "/ZoneGroupTopology/Control";
const ZGT_SVC: &str = "urn:schemas-upnp-org:service:ZoneGroupTopology:1";

#[derive(Clone, Debug, Default)]
pub struct Player {
    pub udn: String,
    pub name: String,
    pub model: String,
    pub ip: String,
    pub port: u16,
    pub is_coordinator: bool,
    pub group_id: String,
    /// the other players in this zone's group
    pub members: Vec<String>,
    /// players this zone drives (sub, surrounds); empty unless it is the
    /// coordinator of a home-theater pair
    pub satellites: Vec<String>,
    pub volume: Option<u32>,
    /// true for subs/surrounds: driven by a zone, they cannot be sent a stream
    /// of their own.  Named this way so `Default` (false) means "playable".
    pub satellite: bool,
}

impl Player {
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            format!("{} ({})", self.ip, self.udn)
        } else {
            format!("{} [{}]", self.name, self.ip)
        }
    }
    pub fn matches(&self, needle: &str) -> bool {
        !needle.is_empty()
            && (self.udn == needle || self.ip == needle || contains_ci(&self.name, needle))
    }
}

// --------------------------------------------------------------------------- //
// discovery
// --------------------------------------------------------------------------- //

/// Parse one SSDP response header line.  Only `LOCATION:` carries the address
/// we need; the value is `http://<host>[:<port>]/xml/device_description.xml`.
///
/// `get()` rather than slicing: a byte index that is not a UTF-8 char boundary
/// would panic, and the buffer went through `from_utf8_lossy`.
pub(crate) fn parse_location(line: &str) -> Option<(String, u16)> {
    let head = line.get(..9)?;
    if !head.eq_ignore_ascii_case("LOCATION:") {
        return None;
    }
    let url = line.get(9..)?.trim().strip_prefix("http://")?;
    let hostport = url.split('/').next().unwrap_or("");
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(1400)),
        None => (hostport.to_string(), 1400),
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

fn ssdp_locations(wait: Duration) -> Vec<(String, u16)> {
    let mut found: Vec<(String, u16)> = Vec::new();
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            log::info("sonos", &format!("SSDP bind failed: {}", e));
            return found;
        }
    };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_multicast_ttl_v4(4);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(350)));

    let msg = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {}\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {}\r\n\r\n",
        SSDP_ADDR, TARGET
    );
    let dest: SocketAddr = SSDP_ADDR.parse().unwrap();
    if let Err(e) = sock.send_to(msg.as_bytes(), dest) {
        log::info("sonos", &format!("SSDP multicast send failed: {}", e));
        return found;
    }
    // one broadcast attempt as well: some setups drop the multicast query
    if let Ok(bcast) = "255.255.255.255:1900".parse::<SocketAddr>() {
        let _ = sock.send_to(msg.as_bytes(), bcast);
    }

    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        match sock.recv(&mut buf) {
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if !text.contains("ZonePlayer") {
                    continue;
                }
                for line in text.lines() {
                    if let Some((host, port)) = parse_location(line) {
                        if !found.iter().any(|(h, _)| *h == host) {
                            found.push((host, port));
                        }
                    }
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                {
                    continue;
                }
                break;
            }
        }
    }
    found
}

fn describe(ip: &str, port: u16) -> Option<Player> {
    let resp = http_get(ip, port, "/xml/device_description.xml", Duration::from_millis(1500)).ok()?;
    if resp.status != 200 {
        return None;
    }
    let udn = xml::tag_value(&resp.body, "UDN")?;
    let mut p = Player {
        udn,
        ip: ip.to_string(),
        port,
        name: xml::tag_value(&resp.body, "roomName")
            .or_else(|| xml::tag_value(&resp.body, "modelName"))
            .unwrap_or_default(),
        model: xml::tag_value(&resp.body, "modelName").unwrap_or_default(),
        ..Default::default()
    };
    if p.name.is_empty() {
        p.name = xml::tag_value(&resp.body, "friendlyName").unwrap_or_default();
    }
    Some(p)
}

#[derive(Clone, Debug)]
pub struct Group {
    pub id: String,
    pub coordinator: String,
    pub members: Vec<String>,
    /// players driven by the coordinator (sub, surrounds) - not zones
    pub satellites: Vec<String>,
}

/// `HTSatChanMapSet="UDN:LF,RF;UDN:SW"` and friends name the players that are
/// driven by a zone instead of being zones themselves.
fn satellites_of(block: &str, coordinator: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for attr in ["HTSatChanMapSet", "Satellites", "SatelliteSerialNum"] {
        for v in xml::all_attr_values(block, "ZoneGroupMember", attr) {
            for part in v.split(';') {
                let udn = part.split(':').next().unwrap_or("").trim().to_string();
                if udn.is_empty() || udn == coordinator {
                    continue;
                }
                if !out.iter().any(|s| s == &udn) {
                    out.push(udn);
                }
            }
        }
    }
    out
}

/// Sonos reports a UDN as `uuid:RINCON_...` in `device_description.xml` but as
/// a bare `RINCON_...` inside ZoneGroupState; compare on the bare form.
pub(crate) fn norm_udn(u: &str) -> String {
    let t = u.trim();
    t.strip_prefix("uuid:").unwrap_or(t).to_string()
}

/// Parse `<ZoneGroupState>` content.
///
/// Current Sonos firmware writes attributes:
/// `<ZoneGroup Coordinator="RINCON_.." ID="RINCON_..:123"><ZoneGroupMember UUID="RINCON_.." ZoneName=".."/></ZoneGroup>`.
/// Older dumps used `<ZonePlayerUUID>` child elements - still accepted.
pub fn parse_groups(zg: &str) -> Vec<Group> {
    let mut out = Vec::new();
    for at in xml::tag_positions(zg, "ZoneGroup") {
        let tail = &zg[at..];
        // `</ZoneGroups>` never matches this needle, so the block stays exact
        let end = tail.find("</ZoneGroup>").unwrap_or(tail.len());
        let block = &tail[..end];
        let id = xml::attr_value(block, "ZoneGroup", "ID")
            .or_else(|| xml::attr_value(block, "ZoneGroup", "Id"))
            .or_else(|| xml::attr_value(block, "ZoneGroup", "id"))
            .or_else(|| xml::tag_value(block, "ZoneGroupID"))
            .unwrap_or_default();
        let mut coordinator = xml::attr_value(block, "ZoneGroup", "Coordinator")
            .or_else(|| xml::tag_value(block, "Coordinator"))
            .unwrap_or_default();
        let mut members = xml::all_attr_values(block, "ZoneGroupMember", "UUID");
        if members.is_empty() {
            members = xml::all_tag_values(block, "ZonePlayerUUID");
        }
        if coordinator.is_empty() {
            if let Some(first) = members.first() {
                coordinator = first.clone();
            }
        }
        if !coordinator.is_empty() && !members.is_empty() {
            let satellites = satellites_of(block, &coordinator);
            out.push(Group {
                id,
                coordinator,
                members,
                satellites,
            });
        }
    }
    out
}

/// Ask any reachable player for the whole-house zone grouping.
pub fn zone_groups(ip: &str, port: u16) -> Vec<Group> {
    match soap_call(
        ip,
        port,
        ZGT,
        ZGT_SVC,
        "GetZoneGroupState",
        &[],
        Duration::from_millis(2000),
    ) {
        Ok(body) => {
            // the ZoneGroupState payload is XML-escaped inside the SOAP reply,
            // so this must unescape (tag_value) rather than take it raw
            let zg = xml::tag_value(&body, "ZoneGroupState").unwrap_or(body);
            parse_groups(&zg)
        }
        Err(e) => {
            log::debug("sonos", &format!("GetZoneGroupState failed: {}", e));
            Vec::new()
        }
    }
}

pub fn discover(wait: Duration) -> Vec<Player> {
    let hosts = ssdp_locations(wait);
    let mut players: Vec<Player> = Vec::new();
    for (ip, port) in hosts.iter().take(24) {
        if let Some(p) = describe(ip, *port) {
            players.push(p);
        }
    }
    if let Some(first) = players.iter().find(|p| !p.udn.is_empty()) {
        let groups = zone_groups(&first.ip, first.port);
        for p in players.iter_mut() {
            let mine = norm_udn(&p.udn);
            // a satellite is matched through its zone's group, not as a member
            if let Some(g) = groups.iter().find(|g| {
                g.members
                    .iter()
                    .chain(g.satellites.iter())
                    .any(|m| norm_udn(m) == mine)
            }) {
                p.group_id = g.id.clone();
                p.is_coordinator = norm_udn(&g.coordinator) == mine;
                p.members = g.members.clone();
                p.satellites = g.satellites.clone();
                p.satellite = g.satellites.iter().any(|s| norm_udn(s) == mine);
            }
        }
    }
    players.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    players
}

/// Live transport state, for the UI.
pub fn transport_state(p: &Player) -> Option<String> {
    let body = soap_call(
        &p.ip,
        p.port,
        AV,
        AV_SVC,
        "GetTransportInfo",
        &[("InstanceID", String::from("0"))],
        Duration::from_millis(1200),
    )
    .ok()?;
    xml::tag_value(&body, "CurrentTransportState")
}

pub fn get_volume(p: &Player) -> Option<u32> {
    let body = soap_call(
        &p.ip,
        p.port,
        RC,
        RC_SVC,
        "GetVolume",
        &[("InstanceID", String::from("0")), ("Channel", String::from("Master"))],
        Duration::from_millis(1200),
    )
    .ok()?;
    xml::tag_value(&body, "CurrentVolume")
        .and_then(|v| v.parse().ok())
}

pub fn set_volume(p: &Player, volume: u32) -> Res<()> {
    let v = volume.min(100).to_string();
    soap_call(
        &p.ip,
        p.port,
        RC,
        RC_SVC,
        "SetVolume",
        &[
            ("InstanceID", String::from("0")),
            ("Channel", String::from("Master")),
            ("DesiredVolume", v),
        ],
        Duration::from_millis(1500),
    )?;
    Ok(())
}

pub fn set_mute(p: &Player, mute: bool) -> Res<()> {
    soap_call(
        &p.ip,
        p.port,
        RC,
        RC_SVC,
        "SetMute",
        &[
            ("InstanceID", String::from("0")),
            ("Channel", String::from("Master")),
            ("DesiredMute", if mute { "1".into() } else { "0".into() }),
        ],
        Duration::from_millis(1500),
    )?;
    Ok(())
}

pub fn stop(p: &Player) -> Res<()> {
    soap_call(
        &p.ip,
        p.port,
        AV,
        AV_SVC,
        "Stop",
        &[("InstanceID", String::from("0"))],
        Duration::from_millis(1500),
    )?;
    Ok(())
}

/// Make this player a standalone zone (undo "join group") - opt-in only.
pub fn become_standalone(p: &Player) -> Res<()> {
    soap_call(
        &p.ip,
        p.port,
        AV,
        AV_SVC,
        "BecomeCoordinatorOfStandaloneGroup",
        &[("InstanceID", String::from("0"))],
        Duration::from_millis(2000),
    )?;
    Ok(())
}

/// DIDL-Lite metadata for a live radio item.
///
/// Two things the player is picky about: the real DIDL-Lite namespace (not the
/// old `device-2000-DIDL-lite-1` spelling) and an `audioBroadcast` item, which
/// is what a never-ending stream is. The value is inserted as a SOAP argument,
/// and soap_body() escapes argument values once - so the inner text is kept
/// verbatim here rather than pre-escaped.
pub fn didl_radio(uri: &str, title: &str, ctype: &str) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
         <item id=\"0\" parentID=\"0\" restricted=\"1\">\
         <dc:title>{}</dc:title>\
         <res protocolInfo=\"http-get:*:{}:*\">{}</res>\
         <upnp:class>object.item.audioItem.audioBroadcast</upnp:class>\
         </item></DIDL-Lite>",
        title, ctype, uri
    )
}

pub fn mime_for(uri: &str) -> &'static str {
    let name = uri.split('?').next().unwrap_or(uri).rsplit('/').next().unwrap_or("").to_string();
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "flac" => "audio/x-flac",
        "ogg" => "audio/ogg",
        _ => "audio/mpeg",
    }
}

pub fn play(p: &Player, uri: &str, title: &str) -> Res<()> {
    let meta = didl_radio(uri, title, mime_for(uri));
    soap_call(
        &p.ip,
        p.port,
        AV,
        AV_SVC,
        "SetAVTransportURI",
        &[
            ("InstanceID", String::from("0")),
            // Sonos does not use the UPnP argument names here: URI/Metadata is
            // refused with UPnPError 402, the player wants CurrentURI and
            // CurrentURIMetaData.
            ("CurrentURI", uri.to_string()),
            ("CurrentURIMetaData", meta),
        ],
        // Generous: the player probes the stream while it answers this call.
        Duration::from_millis(8000),
    )?;
    soap_call(
        &p.ip,
        p.port,
        AV,
        AV_SVC,
        "Play",
        &[
            ("InstanceID", String::from("0")),
            // Speed is required by the Sonos build of this action and it only
            // accepts the plain "1": "1.000000" or an empty value is refused
            // with UPnPError 717.
            ("Speed", String::from("1")),
        ],
        // Play blocks until the player has buffered enough to start, which on
        // a real speaker is well past three seconds.
        Duration::from_millis(8000),
    )?;
    // keep the queue behaviour boring: no shuffle, no repeat
    let _ = soap_call(
        &p.ip,
        p.port,
        AV,
        AV_SVC,
        "SetPlayMode",
        &[
            ("InstanceID", String::from("0")),
            ("NewPlayMode", String::from("NORMAL")),
        ],
        Duration::from_millis(1200),
    );
    Ok(())
}

// --------------------------------------------------------------------------- //
// engine
// --------------------------------------------------------------------------- //

/// Resolve wanted names/UDNs into concrete players, sending group playback to
/// the coordinator of each group.
pub(crate) fn resolve(shared: &Shared) -> Vec<Player> {
    let all = shared.players();
    let wanted = shared.wanted();
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Player> = Vec::new();
    for w in wanted.iter() {
        if w == "all" || w == "*" {
            for p in all.iter() {
                if !p.satellite && (p.is_coordinator || p.group_id.is_empty()) {
                    push_unique(&mut out, p.clone());
                }
            }
            continue;
        }
        let found: Vec<Player> = all.iter().filter(|p| p.matches(w)).cloned().collect();
        if found.is_empty() {
            continue;
        }
        for p in found {
            // a sub/surround shares its zone's room name: aim at the zone that
            // drives it, and never at the satellite itself
            if p.satellite {
                if let Some(coord) = coordinator_for(&all, &p.udn) {
                    push_unique(&mut out, coord);
                }
                continue;
            }
            if !p.is_coordinator && !p.members.is_empty() {
                if let Some(coord) = coordinator_for(&all, &p.udn) {
                    push_unique(&mut out, coord);
                    continue;
                }
            }
            push_unique(&mut out, p);
        }
    }
    out
}

/// The coordinator of the group (or home-theater pair) that drives `udn`.
fn coordinator_for(all: &[Player], udn: &str) -> Option<Player> {
    let mine = norm_udn(udn);
    all.iter()
        .find(|c| {
            c.is_coordinator
                && c.members
                    .iter()
                    .chain(c.satellites.iter())
                    .any(|m| norm_udn(m) == mine)
        })
        .cloned()
}

fn push_unique(list: &mut Vec<Player>, p: Player) {
    if !list.iter().any(|x| x.udn == p.udn) {
        list.push(p);
    }
}

fn key_of(list: &[Player]) -> String {
    list.iter().map(|p| p.udn.as_str()).collect::<Vec<_>>().join(",")
}

pub fn stop_all(shared: &Shared) {
    let active = shared.active();
    for p in active.iter() {
        if let Err(e) = stop(p) {
            log::debug("sonos", &format!("stop {} failed: {}", p.label(), e));
        }
    }
    shared.set_active(Vec::new());
    shared.set_sonos_state("idle");
}

pub fn run(shared: Arc<Shared>) {
    let mut next_refresh = Instant::now();
    let mut retry_at = Instant::now();
    let mut backoff: u64 = 2000;
    let mut pushed_uri = String::new();
    let mut pushed_key = String::new();
    // when the last connection to our stream closed (see the watchdog below)
    let mut unpulled_since: Option<Instant> = None;

    while shared.running() {
        let now = Instant::now();
        if now >= next_refresh {
            next_refresh = now + Duration::from_secs(45);
            let found = discover(Duration::from_millis(1200));
            if !found.is_empty() {
                let mut merged = found;
                // keep volumes we already know
                let old = shared.players();
                for p in merged.iter_mut() {
                    if let Some(o) = old.iter().find(|o| o.udn == p.udn) {
                        p.volume = o.volume;
                    }
                }
                log::debug("sonos", &format!("{} speakers visible", merged.len()));
                shared.set_players(merged);
            }
        }

        let want_sonos = shared.has_sonos();
        let active = shared.active();

        if !want_sonos {
            if !active.is_empty() {
                stop_all(&shared);
                pushed_uri.clear();
                pushed_key.clear();
            }
            std::thread::sleep(Duration::from_millis(400));
            continue;
        }

        if active.is_empty() && now < retry_at {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }

        let uri = shared.stream_url();
        if uri.is_empty() {
            shared.set_sonos_state("no-stream");
            shared.set_error("stream server is not listening yet");
            retry_at = Instant::now() + Duration::from_secs(3);
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        if shared.wanted().is_empty() {
            shared.set_sonos_state("no-speaker");
            if active.is_empty() {
                shared.note("no speaker selected - pick one in the UI or: sonos-bridge player <name>");
            }
            std::thread::sleep(Duration::from_millis(700));
            continue;
        }

        let targets = resolve(&shared);
        if targets.is_empty() {
            shared.set_sonos_state("no-speaker");
            if active.is_empty() {
                shared.set_error("selected speaker not found - run: sonos-bridge discover");
            }
            retry_at = Instant::now() + Duration::from_secs(5);
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }

        let key = key_of(&targets);
        let same = key == pushed_key && uri == pushed_uri && !active.is_empty();
        if same {
            // Watchdog. A player that gives up on a stream keeps reporting
            // PLAYING for a while and then stops asking for the URI, which
            // looks exactly like success from here: transport says PLAYING,
            // nobody is pulling, and the room stays silent for ever. The
            // connection count is the only honest sign that audio is moving, so
            // if nothing has pulled the stream for a while and the player still
            // claims to be playing, hand it the URI again.
            const UNPULLED_LIMIT: Duration = Duration::from_secs(12);
            let pulling = shared.stats.stream_clients.load(std::sync::atomic::Ordering::Relaxed) > 0;
            if pulling {
                // a player that closes and immediately reconnects is normal
                unpulled_since = None;
            } else {
                match unpulled_since {
                    None => unpulled_since = Some(Instant::now()),
                    Some(t) if t.elapsed() >= UNPULLED_LIMIT => {
                        // only chase a player that still claims to be playing:
                        // if the room paused it in the app, leave it alone
                        let claims_playing = active
                            .iter()
                            .any(|p| transport_state(p).as_deref() == Some("PLAYING"));
                        if claims_playing {
                            log::info(
                                "sonos",
                                "nothing has pulled the stream for 12s while the player says \
                                 PLAYING - re-arming the URI",
                            );
                            shared.set_active(Vec::new());
                        } else {
                            log::debug(
                                "sonos",
                                "stream idle and the player is not playing: leaving it stopped",
                            );
                        }
                        unpulled_since = None;
                    }
                    Some(_) => {}
                }
            }
            std::thread::sleep(Duration::from_millis(900));
            continue;
        }

        if !active.is_empty() {
            stop_all(&shared);
        }
        shared.set_sonos_state("starting");
        let title = "sonos-bridge".to_string();
        let mut ok: Vec<Player> = Vec::new();
        let mut last: Option<String> = None;
        for p in targets.iter() {
            match play(p, &uri, &title) {
                Ok(()) => {
                    log::info("sonos", &format!("streaming to {}", p.label()));
                    ok.push(p.clone());
                }
                Err(e) => {
                    last = Some(e.clone());
                    log::info("sonos", &format!("could not start {}: {}", p.label(), e));
                }
            }
        }
        if ok.is_empty() {
            shared.set_active(Vec::new());
            shared.set_sonos_state("error");
            shared.set_error(&last.unwrap_or_else(|| "no speaker accepted the stream".to_string()));
            pushed_key.clear();
            pushed_uri.clear();
            retry_at = Instant::now() + Duration::from_millis(backoff);
            backoff = (backoff * 2).min(20_000);
        } else {
            shared.set_active(ok);
            shared.set_sonos_state("playing");
            let _ = shared
                .stats
                .sonos_plays
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            pushed_key = key;
            pushed_uri = uri;
            backoff = 2000;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    stop_all(&shared);
    log::info("sonos", "engine stopped");
}
