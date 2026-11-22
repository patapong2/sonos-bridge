//! Offline self-checks for the parts that do not need audio hardware or a
//! speaker: framing, buffering, parsing, command dispatch.
//!
//! Run with:  sonos-bridge selftest

use std::io::Cursor;
use std::time::Duration;

use crate::config::Config;
use crate::{cmds, json, net, sonos, state, util, xml};

pub fn run() -> i32 {
    let mut total = 0usize;
    let mut fails: Vec<String> = Vec::new();
    let mut check = |name: &str, ok: bool| {
        total += 1;
        if ok {
            println!("  ok    {}", name);
        } else {
            println!("  FAIL  {}", name);
            fails.push(name.to_string());
        }
    };

    // ---- xml ------------------------------------------------------------ //
    let doc = "<root><UDN>uuid:ZonePlayer-ABC</UDN><roomName>Living Room</roomName>\
               <ZoneGroupState>&lt;ZoneGroups/&gt;</ZoneGroupState>\
               <svc><Text>a &amp; b</Text></svc></root>";
    check(
        "xml: tag value",
        xml::tag_value(doc, "roomName").as_deref() == Some("Living Room"),
    );
    check(
        "xml: ignores longer names",
        xml::tag_value(doc, "room").is_none(),
    );
    check(
        "xml: unescapes",
        xml::tag_value(doc, "Text").as_deref() == Some("a & b"),
    );
    check(
        "xml: entity in value",
        xml::tag_value(doc, "UDN").as_deref() == Some("uuid:ZonePlayer-ABC"),
    );
    let zg = "<ZG><ZonePlayerUUID>uuid:A</ZonePlayerUUID><ZonePlayerUUID>uuid:B</ZonePlayerUUID></ZG>";
    check("xml: repeated tags", xml::all_tag_values(zg, "ZonePlayerUUID").len() == 2);
    check(
        "xml: attribute",
        xml::attr_value("<Thing name=\"x\" id=\"7\">", "Thing", "name").as_deref() == Some("x"),
    );

    // ---- sonos grouping -------------------------------------------------- //
    let groups = sonos::parse_groups(
        "<ZoneGroupState><ZoneGroups><ZoneGroup Coordinator=\"uuid:A\" Id=\"Z1:0\">\
         <ZoneGroupID>uuid:A:0</ZoneGroupID>\
         <ZonePlayerUUID>uuid:A</ZonePlayerUUID><ZonePlayerUUID>uuid:B</ZonePlayerUUID>\
         </ZoneGroup></ZoneGroups></ZoneGroupState>",
    );
    check(
        "sonos: one group parsed",
        groups.len() == 1
            && groups[0].coordinator == "uuid:A"
            && groups[0].members.len() == 2,
    );

    // the payload Sonos actually sends (XML-escaped inside the SOAP reply):
    // ZoneGroupMember attributes, uppercase ID, bare RINCON ids
    let escaped = "&lt;ZoneGroupState&gt;&lt;ZoneGroups&gt;&lt;ZoneGroup Coordinator=&quot;RINCON_BBB&quot; ID=&quot;RINCON_BBB:3541141985&quot;&gt;&lt;ZoneGroupMember UUID=&quot;RINCON_BBB&quot; ZoneName=&quot;Bedroom&quot; /&gt;&lt;ZoneGroupMember UUID=&quot;RINCON_SSS&quot; ZoneName=&quot;Bedroom&quot; /&gt;&lt;/ZoneGroup&gt;&lt;/ZoneGroups&gt;&lt;/ZoneGroupState&gt;";
    let real = sonos::parse_groups(&util::xml_unescape(escaped));
    check(
        "sonos: real escaped ZoneGroupState parsed",
        real.len() == 1
            && real[0].coordinator == "RINCON_BBB"
            && real[0].id == "RINCON_BBB:3541141985",
    );
    check(
        "sonos: ZoneGroupMember UUID attributes read",
        real.first().map(|g| g.members.len()) == Some(2),
    );
    check(
        "sonos: ZoneGroups/ZoneGroupState are not a ZoneGroup",
        sonos::parse_groups("<ZoneGroupState><ZoneGroups></ZoneGroups></ZoneGroupState>").is_empty(),
    );
    check(
        "sonos: uuid: prefix normalised for matching",
        sonos::norm_udn("uuid:RINCON_X") == "RINCON_X" && sonos::norm_udn("RINCON_X") == "RINCON_X",
    );

    // home-theater pair: a sub is a satellite of the zone, not a zone itself
    let ht = "<ZoneGroupState><ZoneGroups><ZoneGroup Coordinator=\"RINCON_BEAM\" ID=\"RINCON_BEAM:1\"><ZoneGroupMember UUID=\"RINCON_BEAM\" ZoneName=\"Bedroom\" HTSatChanMapSet=\"RINCON_BEAM:LF,RF;RINCON_SUB:SW\" /></ZoneGroup></ZoneGroups></ZoneGroupState>";
    let htg = sonos::parse_groups(ht);
    check(
        "sonos: satellite read from HTSatChanMapSet",
        htg.len() == 1 && htg[0].satellites == vec!["RINCON_SUB".to_string()],
    );
    check(
        "sonos: coordinator is not its own satellite",
        htg.first().map(|g| g.satellites.len()) == Some(1),
    );

    // ---- play-target resolution ------------------------------------------ //
    {
        let mut cfg = Config::default();
        cfg.stream_token = "tok".to_string();
        let sh = state::Shared::new(cfg);
        let beam = sonos::Player {
            udn: "uuid:RINCON_BEAM".to_string(),
            name: "Bedroom".to_string(),
            ip: "192.168.0.5".to_string(),
            is_coordinator: true,
            group_id: "RINCON_BEAM:1".to_string(),
            members: vec!["RINCON_BEAM".to_string()],
            satellites: vec!["RINCON_SUB".to_string()],
            satellite: false,
            ..Default::default()
        };
        let sub = sonos::Player {
            udn: "uuid:RINCON_SUB".to_string(),
            name: "Bedroom".to_string(),
            ip: "192.168.0.29".to_string(),
            group_id: "RINCON_BEAM:1".to_string(),
            members: vec!["RINCON_BEAM".to_string()],
            satellite: true,
            ..Default::default()
        };
        sh.set_players(vec![beam, sub]);
        sh.set_wanted(vec!["Bedroom".to_string()]);
        let t = sonos::resolve(&sh);
        check(
            "resolve: room shared by zone+sub targets the zone only",
            t.len() == 1 && t[0].udn == "uuid:RINCON_BEAM",
        );
        sh.set_wanted(vec!["all".to_string()]);
        let t2 = sonos::resolve(&sh);
        check(
            "resolve: 'all' skips satellites",
            t2.len() == 1 && t2[0].udn == "uuid:RINCON_BEAM",
        );
        sh.set_wanted(vec!["192.168.0.29".to_string()]);
        let t3 = sonos::resolve(&sh);
        check(
            "resolve: naming the sub still routes to its zone",
            t3.len() == 1 && t3[0].udn == "uuid:RINCON_BEAM",
        );
    }

    // ---- ssdp LOCATION parsing ------------------------------------------- //
    let loc = |l: &str| sonos::parse_location(l).map(|(h, p)| (h, p));
    check(
        "ssdp: LOCATION with port",
        loc("LOCATION: http://192.168.0.29:1400/xml/device_description.xml")
            == Some(("192.168.0.29".to_string(), 1400u16)),
    );
    check(
        "ssdp: LOCATION without port defaults to 1400",
        loc("Location: http://192.168.0.5/xml/device_description.xml")
            == Some(("192.168.0.5".to_string(), 1400u16)),
    );
    check(
        "ssdp: LOCATION, no space after colon",
        loc("LOCATION:http://10.0.0.9:1400/x.xml") == Some(("10.0.0.9".to_string(), 1400u16)),
    );
    check("ssdp: ignores ST line", sonos::parse_location("ST: urn:schemas-upnp-org:device:ZonePlayer:1").is_none());
    check("ssdp: ignores a longer name", sonos::parse_location("LOCATIONX: http://a/1").is_none());
    check("ssdp: ignores short/truncated line", sonos::parse_location("LOCATIO").is_none());
    check("ssdp: ignores non-http scheme", sonos::parse_location("LOCATION: https://a/1").is_none());

    // ---- soap ------------------------------------------------------------ //
    let body = net::soap_body(
        "urn:schemas-upnp-org:service:AVTransport:1",
        "SetAVTransportURI",
        &[
            ("InstanceID", String::from("0")),
            ("URI", String::from("http://1.2.3.4:1/stream.mp3?t=x")),
            ("Metadata", String::from("<dc:title>a<b>&c</dc:title>")),
        ],
    );
    check(
        "soap: envelope + escaping",
        body.contains("<u:SetAVTransportURI")
            && body.contains("&lt;dc:title&gt;a&lt;b&gt;&amp;c&lt;/dc:title&gt;")
            && body.ends_with("</s:Envelope>"),
    );

    // ---- http ------------------------------------------------------------ //
    let raw = b"GET /stream.mp3?t=abc&x=1 HTTP/1.1\r\nHost: 192.168.1.5:80\r\nUser-Agent: Sonos\r\n\r\n";
    let mut cur = Cursor::new(raw.to_vec());
    match net::read_request(&mut cur) {
        Ok(req) => check(
            "http: request parse",
            req.path == "/stream.mp3"
                && req.query == "t=abc&x=1"
                && req
                    .headers
                    .iter()
                    .any(|(k, v)| k == "user-agent" && v == "Sonos"),
        ),
        Err(e) => {
            println!("  http parse failed: {}", e);
            check("http: request parse", false);
        }
    }

    // ---- chunked transfer (Sonos sends chunked bodies) -------------------- //
    let dc = |b: &[u8]| String::from_utf8_lossy(&net::decode_chunked(b)).to_string();
    check(
        "http: dechunk basic",
        dc(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n") == "hello world",
    );
    check(
        "http: dechunk multi-kb hex size",
        dc(&{
            let mut v = format!("{:x}\r\n", 300).into_bytes();
            v.extend_from_slice(&vec![b'x'; 300]);
            v.extend_from_slice(b"\r\n0\r\n\r\n");
            v
        }) == "x".repeat(300),
    );
    check(
        "http: dechunk ignores chunk extension",
        dc(b"3;ext=1\r\nabc\r\n0\r\n\r\n") == "abc",
    );
    // truncated stream: keep the payload bytes we did get, never the framing
    let trunc = dc(b"6\r\nabcdef\r\n6\r\ntrunc");
    check("http: dechunk truncated keeps payload", trunc == "abcdeftrunc");
    // the failure that actually bit us: <roomName> split across a chunk boundary
    let sonosish = dc(b"19\r\n<root><device><roomName>B\r\n11\r\nedroom</roomName>\r\n0\r\n\r\n");
    check(
        "http: roomName survives a chunk boundary",
        xml::tag_value(&sonosish, "roomName").as_deref() == Some("Bedroom"),
    );
    check("http: dechunk non-chunked input yields nothing", dc(b"<root/>") == "");
    {
        // end-to-end: a genuine chunked response on loopback, through http_get
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok();
        if let Some(l) = listener {
            let port = l.local_addr().map(|a| a.port()).unwrap_or(0);
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                if let Ok((mut sock, _)) = l.accept() {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf);
                    let _ = sock.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nTransfer-Encoding: chunked\r\n\r\n\
                          8\r\n<root><a\r\n8\r\n>b</a></\r\n5\r\nroot>\r\n0\r\n\r\n",
                    );
                    let _ = sock.flush();
                }
            });
            match net::http_get("127.0.0.1", port, "/x", std::time::Duration::from_millis(3000)) {
                Ok(r) => check(
                    "http: chunked GET decoded end-to-end",
                    r.status == 200 && r.body == "<root><a>b</a></root>",
                ),
                Err(e) => {
                    println!("  chunked GET failed: {}", e);
                    check("http: chunked GET decoded end-to-end", false);
                }
            }
        } else {
            println!("  (skipped chunked end-to-end: cannot bind loopback)");
        }
    }

    // ---- mpeg framing ---------------------------------------------------- //
    // MPEG-1 layer III, 256 kbit/s, 48 kHz -> 768 byte frame
    let hdr = [0xFFu8, 0xFB, 0x64, 0x00];
    check("mpeg: frame length", net::mpeg_frame_len(&hdr) == Some(768));
    check(
        "mpeg: rejects reserved layer",
        net::mpeg_frame_len(&[0xFF, 0xF1, 0x64, 0x00]).is_none(),
    );
    let mut junk: Vec<u8> = vec![0xAA; 10];
    junk.extend_from_slice(&hdr);
    junk.extend_from_slice(&[0u8; 800]);
    check("mpeg: finds frame after junk", net::first_frame(&junk) == Some(10));
    let mut id3 = b"ID3\x03\x00\x00\x00".to_vec();
    id3.extend_from_slice(&[0x00, 0x04, 0x00]); // size 512 (synchsafe at bytes 6-9)
    check("id3: tag length", net::id3_len(&id3) == Some(522));

    // ---- buffers --------------------------------------------------------- //
    let log = crate::queues::AudioLog::new(50);
    let data: Vec<u8> = (0..100u8).collect();
    log.push(&data);
    let (pos, bytes) = log.read_from(0, 10);
    check(
        "log: resyncs a lagging reader",
        pos == 50 && bytes.is_empty() && log.dropped() == 50,
    );
    let (pos2, bytes2) = log.read_from(90, 10);
    check(
        "log: tail read",
        pos2 == 100 && bytes2.len() == 10 && bytes2[0] == 90,
    );
    let (pos3, bytes3) = log.read_from(100, 10);
    check("log: nothing new", pos3 == 100 && bytes3.is_empty());
    // a reader still inside the retained window must be served normally, or the
    // stream server would hang up on a healthy speaker
    let (pos4, bytes4) = log.read_from(60, 10);
    check(
        "log: a reader inside the window is served, not bumped",
        pos4 == 70 && bytes4.len() == 10 && bytes4[0] == 60,
    );
    let (pos5, bytes5) = log.read_from(95, 10);
    check(
        "log: a reader at the live edge gets what exists",
        pos5 == 100 && bytes5.len() == 5,
    );

    // ring capacity: it is a *time* budget. too small and a player that pauses
    // reading (what radio players do while they buffer) is treated as overrun,
    // which splices a hole into its audio - the stutter that ends in silence.
    let mut lcfg = Config::default();
    lcfg.stream_format = "mp3".to_string();
    lcfg.stream_bitrate = "192k".to_string();
    let cap = state::log_cap(&lcfg);
    check(
        "log cap: 60s of 192 kbit/s",
        cap == (192 * 1000 / 8) as usize * state::LAG_SECONDS as usize,
    );
    check(
        "log cap: holds at least 30s of audio",
        cap >= (192 * 1000 / 8) as usize * 30,
    );
    lcfg.stream_bitrate = "64k".to_string();
    check(
        "log cap: small bitrate still keeps a usable floor",
        state::log_cap(&lcfg) >= 256_000,
    );
    lcfg.stream_bitrate = "nonsense".to_string();
    check(
        "log cap: garbage bitrate falls back to 192k",
        state::log_cap(&lcfg) == cap,
    );
    lcfg.stream_format = "flac".to_string();
    check(
        "log cap: flac stays inside the memory ceiling",
        state::log_cap(&lcfg) <= 4_000_000,
    );

    let q = crate::queues::ByteQueue::new(10);
    q.push(&[1, 2, 3, 4, 5, 6]);
    q.push(&[11, 12, 13, 14, 15, 16]);
    let mut out: Vec<u8> = Vec::new();
    let n = q.pop_up_to(&mut out, 4);
    check(
        "queue: drop-oldest keeps latency bounded",
        q.len() == 6 && n == 4 && out[0] == 3,
    );
    // the drops must be visible: a queue that quietly eats audio shows up as
    // stutter in the room and as nothing at all in the counters
    check("queue: dropped bytes are counted", q.lost() == 2);
    let q2 = crate::queues::ByteQueue::new(16);
    q2.push(&[7u8; 16]);
    check(
        "queue: a burst that fits costs nothing",
        q2.lost() == 0 && q2.len() == 16,
    );
    q2.push(&[7u8; 8]);
    check("queue: a burst past the cap is reported", q2.lost() == 8);
    let mut drain: Vec<u8> = Vec::new();
    while q2.pop_up_to(&mut drain, 4096) > 0 {}
    check(
        "queue: a keeping-up consumer leaves it empty",
        q2.len() == 0 && q2.lost() == 8,
    );

    // ---- net policy ------------------------------------------------------ //
    check(
        "policy: lan ranges",
        util::is_rfc1918("192.168.1.20")
            && util::is_rfc1918("10.0.0.5")
            && util::is_rfc1918("172.20.0.9")
            && util::is_rfc1918("127.0.0.1")
            && !util::is_rfc1918("8.8.8.8")
            && !util::is_rfc1918("garbage"),
    );
    check(
        "policy: token redaction",
        util::redact("http://1.2.3.4:9/s?t=SECRET", "SECRET") == "http://1.2.3.4:9/s?t=<token>",
    );

    // ---- codecs ---------------------------------------------------------- //
    let round = util::percent_decode(&util::percent_encode("Living Room/Bedroom &+x"));
    check("codec: percent round trip", round == "Living Room/Bedroom &+x");
    let pairs = util::parse_query("mode=sonos&player=Living+Room");
    check(
        "codec: query parse",
        util::query_get(&pairs, "player").as_deref() == Some("Living Room"),
    );
    let mut obj = json::Obj::new();
    obj.str("name", "a\"b\nc");
    obj.num("n", 3);
    obj.bool("b", true);
    obj.raw("nul", "null");
    check(
        "json: escaping",
        obj.done() == "{\"name\":\"a\\\"b\\nc\",\"n\":3,\"b\":true,\"nul\":null}",
    );

    // ---- queue waiting -------------------------------------------------- //
    // a live producer delivers at the audio rate; a consumer must wait for it
    // rather than fill the gap with bytes that were never played
    {
        use std::sync::Arc;
        use std::time::Instant;
        let q = Arc::new(crate::queues::ByteQueue::new(1 << 15));
        let late = q.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            late.push(b"12345");
        });
        let mut out: Vec<u8> = Vec::new();
        let t0 = Instant::now();
        let got = q.pop_wait(&mut out, 4096, Duration::from_millis(500));
        check("queue: pop_wait takes data that arrives late", got == 5 && out == b"12345");
        check(
            "queue: pop_wait actually waited instead of returning empty",
            t0.elapsed() >= Duration::from_millis(60),
        );
        let t1 = Instant::now();
        let none = q.pop_wait(&mut out, 4096, Duration::from_millis(80));
        check(
            "queue: pop_wait gives up after its budget",
            none == 0 && t1.elapsed() >= Duration::from_millis(60),
        );
    }

    // ---- soap ----------------------------------------------------------- //
    // The SOAPAction header must be one quoted unit, "<service>#<action>".
    // Sonos firmware 8x/9x still dispatches the action when the header is
    // written "<service>"#<action>, but then rejects every call that carries
    // arguments with UPnPError 402 - so pin the exact bytes on the wire.
    {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::mpsc::channel;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = channel::<String>();
        let srv = std::thread::spawn(move || {
            for i in 0..2u16 {
                let (mut s, _) = match l.accept() {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let mut br = std::io::BufReader::new(s.try_clone().unwrap());
                if let Ok(r) = net::read_request(&mut br) {
                    let _ = tx.send(r.header("soapaction").unwrap_or_default().to_string());
                    let inner = if i == 0 {
                        String::from("<u:Resp>ok</u:Resp>")
                    } else {
                        String::from(
                            "<s:Fault><faultcode>s:Client</faultcode>\
                             <faultstring>UPnPError</faultstring>\
                             <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
                             <errorCode>749</errorCode></UPnPError></detail></s:Fault>",
                        )
                    };
                    let env = format!(
                        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                         <s:Body>{}</s:Body></s:Envelope>",
                        inner
                    );
                    let (code, reason) = if i == 0 {
                        (200u16, "OK")
                    } else {
                        (500, "Internal Server Error")
                    };
                    let _ = net::write_response(&mut s, code, reason, "text/xml", &env);
                    let _ = s.flush();
                    // FIN, not RST: the client may not have read the reply yet
                    let _ = s.shutdown(std::net::Shutdown::Write);
                }
            }
        });
        let svc = "urn:schemas-upnp-org:service:RenderingControl:1";
        let args: Vec<(&str, String)> =
            vec![("InstanceID", String::from("0")), ("Channel", String::from("Master"))];
        let ok = net::soap_call("127.0.0.1", port, "/Ctl", svc, "GetVolume", &args, Duration::from_secs(3));
        let hdr = rx.recv().unwrap_or_default();
        check(
            &format!("soap: SOAPAction is one quoted unit ({})", hdr),
            hdr == format!("\"{}#GetVolume\"", svc),
        );
        check("soap: well-formed call returns the body", ok.is_ok());
        // a fault has to surface its errorCode, otherwise every failure reads the same
        let bad = net::soap_call("127.0.0.1", port, "/Ctl", svc, "Play", &args, Duration::from_secs(3));
        let err = bad.as_ref().unwrap_err().clone();
        check(
            &format!("soap: fault carries the UPnP code ({})", err),
            err.contains("749") && err.contains("URI rejected"),
        );
        let _ = srv.join();
        check(
            "soap: 402 decodes to 'invalid arguments'",
            net::upnp_error_name("402") == "invalid arguments",
        );
    }

    // A server that keeps the connection open must not stall the client: the
    // body is complete once its framing is satisfied, not when the socket closes.
    {
        use std::io::Write;
        use std::net::TcpListener;
        use std::time::Instant;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut br = std::io::BufReader::new(s.try_clone().unwrap());
                let _ = net::read_request(&mut br);
                let body = "<r>hi</r>";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
                // stay open on purpose
                std::thread::sleep(Duration::from_millis(1500));
            }
        });
        let t0 = Instant::now();
        let got = net::http_get("127.0.0.1", port, "/", Duration::from_secs(5));
        let dt = t0.elapsed();
        check(
            &format!(
                "http: takes the framed body without waiting for close ({:?})",
                dt
            ),
            got.map(|x| x.body == "<r>hi</r>").unwrap_or(false) && dt < Duration::from_millis(1200),
        );
        let _ = srv.join();
    }

    // ---- chunked framing -------------------------------------------------- //
    check(
        "chunk: complete only after the zero-size chunk",
        !net::chunked_complete(b"3\r\nabc\r\n") && net::chunked_complete(b"3\r\nabc\r\n0\r\n\r\n"),
    );

    // ---- didl ----------------------------------------------------------- //
    let meta = sonos::didl_radio("http://192.168.0.7:9/stream.mp3?t=k", "Bridge & R&D", "audio/mpeg");
    check(
        "didl: real DIDL-Lite namespace",
        meta.contains("xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\""),
    );
    check(
        "didl: not the bogus device-2000 namespace",
        !meta.contains("device-2000"),
    );
    check(
        "didl: a stream is an audioBroadcast item",
        meta.contains("object.item.audioItem.audioBroadcast"),
    );
    check(
        "didl: res carries protocolInfo + the stream url",
        meta.contains("protocolInfo=\"http-get:*:audio/mpeg:*\"")
            && meta.contains("http://192.168.0.7:9/stream.mp3?t=k"),
    );
    check(
        "didl: mime for flac/ogg/mp3",
        sonos::mime_for("http://h/a.flac") == "audio/x-flac"
            && sonos::mime_for("http://h/a.ogg") == "audio/ogg"
            && sonos::mime_for("http://h/a.mp3?t=1") == "audio/mpeg",
    );
    // the metadata is a SOAP argument value: it must be escaped exactly once,
    // otherwise the player receives &amp;lt; and parses no DIDL at all
    let env = net::soap_body(
        "urn:schemas-upnp-org:service:AVTransport:1",
        "SetAVTransportURI",
        &[("InstanceID", String::from("0")), ("CurrentURIMetaData", meta.clone())],
    );
    check(
        "soap: metadata escaped once, not twice",
        env.contains("&lt;DIDL-Lite") && !env.contains("&amp;lt;"),
    );
    check(
        "soap: title with & survives exactly one escaping pass",
        !env.contains("&amp;amp;") && env.contains("Bridge &amp; R&amp;D"),
    );
    check(
        "soap: Sonos argument names for SetAVTransportURI",
        env.contains("<CurrentURIMetaData>") && env.contains("<InstanceID>0</InstanceID>"),
    );

    // ---- config ---------------------------------------------------------- //
    let mut cfg = Config::default();
    cfg.sink_name = "my sink/x".to_string();
    check("config: sink name sanitised", {
        let _ = cfg.validate();
        cfg.sink_name == "my_sink_x"
    });
    let mut cfg2 = Config::default();
    cfg2.route = "everything".to_string();
    check("config: rejects bad route", cfg2.validate().is_err());
    let mut cfg3 = Config::default();
    check(
        "config: set() knows its keys",
        cfg3.set("stream_format", "flac").unwrap_or(false)
            && !cfg3.set("nope", "1").unwrap_or(true)
            && cfg3.stream_format == "flac",
    );

    // ---- command dispatch ------------------------------------------------ //
    cfg3.stream_token = "tok".to_string();
    let shared = state::Shared::new(cfg3.clone());
    let (ok, text) = cmds::run(&shared, "version");
    check("cmd: version", ok && text.contains(crate::VERSION));
    let (ok, _) = cmds::run(&shared, "notacommand");
    check("cmd: unknown command fails", !ok);
    let (ok, json) = cmds::run(&shared, "json");
    check(
        "cmd: json state",
        ok && json.starts_with('{') && json.ends_with('}') && json.contains("\"app\":\"sonos-bridge\""),
    );
    check(
        "cmd: json is valid-ish (balanced braces)",
        json.matches('{').count() == json.matches('}').count(),
    );
    let (ok, _) = cmds::run(&shared, "route everything");
    check("cmd: route validation", !ok);
    let (ok, _) = cmds::run(&shared, "volume 500");
    check("cmd: volume arg check", !ok);
    let (ok, _) = cmds::run(&shared, "player Living Room");
    let wanted = shared.wanted();
    check(
        "cmd: player selection",
        ok && wanted.len() == 1 && wanted[0] == "Living Room",
    );
    let (ok, help) = cmds::run(&shared, "help");
    check("cmd: help mentions route", ok && help.contains("route <off|sonos|local|both>"));
    check("cmd: help mentions shutdown", help.contains("shutdown"));

    // quoting: only the command line has a shell in front of it, so the socket,
    // the web API and the desktop panel deliver quoted text to us verbatim
    let eq_args = |got: Vec<String>, want: &[&str]| -> bool {
        got.len() == want.len() && got.iter().zip(want.iter()).all(|(a, b)| a == b)
    };
    check(
        "args: plain words",
        eq_args(cmds::split_args("sonos Living Room"), &["sonos", "Living", "Room"]),
    );
    check(
        "args: quotes group a name with spaces",
        eq_args(cmds::split_args("sonos \"Bedroom Beam\""), &["sonos", "Bedroom Beam"]),
    );
    check(
        "args: quoted list keeps its commas",
        eq_args(
            cmds::split_args("sonos \"Bedroom,Office\""),
            &["sonos", "Bedroom,Office"],
        ),
    );
    check(
        "args: no stray quote survives",
        eq_args(cmds::split_args("\"Bedroom,Bedroom\""), &["Bedroom,Bedroom"]),
    );
    check("args: empty input yields nothing", cmds::split_args("   ").is_empty());
    let (ok, _) = cmds::run(&shared, "route sonos \"Bedroom Beam\"");
    let wanted = shared.wanted();
    check(
        "cmd: quoted speaker name reaches wanted intact",
        ok && wanted.len() == 1 && wanted[0] == "Bedroom Beam",
    );
    // deferred so the reply is flushed before the process comes down
    let (ok, _) = cmds::run(&shared, "shutdown");
    check("cmd: shutdown accepted, stop deferred", ok && shared.running());

    println!();
    if fails.is_empty() {
        println!("all {} checks passed", total);
        0
    } else {
        println!("{}/{} checks FAILED:", fails.len(), total);
        for f in fails.iter() {
            println!("  - {}", f);
        }
        1
    }
}
