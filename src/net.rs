//! Minimal HTTP: a client for Sonos' UPnP/SOAP endpoints, request parsing for
//! our own servers, the chunked-transfer codec, and MPEG frame scanning.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::util::{estr, xml_escape, Res};

pub struct Resp {
    pub status: u16,
    pub body: String,
}

fn header_block(extra: &[(&str, String)]) -> String {
    let mut out = String::from("Accept: */*\r\nUser-Agent: sonos-bridge\r\nConnection: close\r\n");
    for (k, v) in extra {
        out.push_str(&format!("{}: {}\r\n", k, v));
    }
    out
}

pub fn http_request(
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    extra: &[(&str, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Res<Resp> {
    let addrs: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(estr)?
        .collect();
    if addrs.is_empty() {
        return Err(estr(format!("cannot resolve {}", host)));
    }
    let mut sock = TcpStream::connect_timeout(&addrs[0], timeout).map_err(estr)?;
    sock.set_read_timeout(Some(timeout)).map_err(estr)?;
    sock.set_write_timeout(Some(timeout)).map_err(estr)?;

    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: {}:{}\r\n{}",
        method,
        path,
        host,
        port,
        header_block(extra)
    );
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    sock.write_all(req.as_bytes()).map_err(estr)?;
    if let Some(b) = body {
        sock.write_all(b).map_err(estr)?;
    }
    sock.flush().map_err(estr)?;

    // Read only what the framing describes: header block, then the body.
    // Reading to end-of-stream would wait for the other side to close, so a
    // keep-alive server turns a healthy reply into a read timeout.
    let mut raw: Vec<u8> = Vec::new();
    let split = loop {
        if let Some(p) = find_sub(&raw, b"\r\n\r\n") {
            break p;
        }
        if raw.len() > MAX_HEAD {
            return Err(estr("response headers too large"));
        }
        if !read_more(&mut sock, &mut raw, timeout)? {
            break raw.len(); // closed before any header block
        }
    };
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut body_bytes: Vec<u8> = if split + 4 <= raw.len() {
        raw[split + 4..].to_vec()
    } else {
        Vec::new()
    };

    // Sonos answers with chunked bodies (the device description and every SOAP
    // reply); decoding those is mandatory or the XML is riddled with chunk sizes.
    if header_of(&head, "transfer-encoding").contains("chunked") {
        while !chunked_complete(&body_bytes) {
            if body_bytes.len() > MAX_BODY {
                return Err(estr("response body too large"));
            }
            if !read_more(&mut sock, &mut body_bytes, timeout)? {
                break; // stream ended early: decode what arrived
            }
        }
    } else if let Some(n) = header_of(&head, "content-length").parse::<usize>().ok() {
        while body_bytes.len() < n {
            if body_bytes.len() > MAX_BODY {
                return Err(estr("response body too large"));
            }
            if !read_more(&mut sock, &mut body_bytes, timeout)? {
                break;
            }
        }
        body_bytes.truncate(n);
    }
    let body = if header_of(&head, "transfer-encoding").contains("chunked") {
        decode_chunked(&body_bytes)
    } else {
        body_bytes
    };

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    let _proto = parts.next();
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok(Resp {
        status,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

/// Index of `needle` in `hay`.
pub fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (0..=(hay.len() - needle.len())).find(|i| &hay[*i..*i + needle.len()] == needle)
}

/// Decode a chunked-transfer body.  Stops at the terminating zero-size chunk;
/// a truncated stream yields what was decoded so far rather than garbage.
/// Response headers we accept from a device, as a safety bound.
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 32 * 1024 * 1024;

/// Value of a header in a raw header block, case-insensitive, lower-cased.
pub fn header_of(head: &str, name: &str) -> String {
    let n = format!("{}:", name.to_ascii_lowercase());
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with(&n))
        .map(|l| l[n.len()..].trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// One more read into `buf`. Ok(false) means end of stream. A read that times
/// out is reported as such instead of leaking the raw EAGAIN to the caller.
fn read_more(sock: &mut TcpStream, buf: &mut Vec<u8>, timeout: Duration) -> Res<bool> {
    let mut chunk = [0u8; 8192];
    loop {
        match sock.read(&mut chunk) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                return Ok(true);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.raw_os_error() == Some(11) =>
            {
                return Err(estr(format!(
                    "no answer from server within {} ms",
                    timeout.as_millis()
                )));
            }
            Err(e) => return Err(estr(e)),
        }
    }
}

/// True once the terminating zero-size chunk is present and complete.
pub fn chunked_complete(b: &[u8]) -> bool {
    walk_chunks(b).1
}

pub fn decode_chunked(b: &[u8]) -> Vec<u8> {
    walk_chunks(b).0
}

fn walk_chunks(b: &[u8]) -> (Vec<u8>, bool) {
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut rest = b;
    loop {
        let nl = match find_sub(rest, b"\r\n") {
            Some(i) => i,
            None => return (out, false),
        };
        let line = String::from_utf8_lossy(&rest[..nl]).to_string();
        // a chunk size may carry an extension: "1a3;ext=x"
        let size_txt = line.split(';').next().unwrap_or("").trim();
        let size = match usize::from_str_radix(size_txt, 16) {
            Ok(v) => v,
            Err(_) => return (out, false),
        };
        if size == 0 {
            return (out, true);
        }
        rest = &rest[nl + 2..];
        let take = size.min(rest.len());
        out.extend_from_slice(&rest[..take]);
        if take < size {
            return (out, false); // truncated
        }
        rest = &rest[take..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
}

pub fn http_get(host: &str, port: u16, path: &str, timeout: Duration) -> Res<Resp> {
    http_request("GET", host, port, path, &[], None, timeout)
}

/// Meaning of the UPnP control-1-0 error codes a Sonos player returns.
pub fn upnp_error_name(code: &str) -> String {
    String::from(match code {
        "401" => "invalid action",
        "402" => "invalid arguments",
        "404" => "invalid argument name",
        "501" => "action failed",
        "600" => "argument value invalid",
        "601" => "argument value out of range",
        "606" => "action not authorized",
        "700" => "object does not exist",
        "701" => "transition not available",
        "702" => "status conflict",
        "703" => "playback error",
        "704" => "resource not available",
        "705" => "action not authorized",
        "706" => "URI stale",
        "707" => "metadata field value invalid",
        "708" => "result not available",
        "709" => "argument mismatch",
        "710" => "argument value argument mismatch",
        "711" => "string argument too long",
        "712" => "send setup failure",
        "713" => "retrieval failure",
        "714" => "invalid instance ID",
        "720" => "URI invalid",
        "727" => "action rejected",
        "749" => "URI rejected by player",
        "800" => "backend call failed",
        "801" => "opensearch error",
        "802" => "not logged in",
        _ => "UPnPError",
    })
}

/// Build a SOAP 1.1 envelope (split out so it can be unit-tested offline).
pub fn soap_body(service: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\n\
         <s:Body>",
    );
    body.push_str(&format!("<u:{} xmlns:u=\"{}\">", action, service));
    for (k, v) in args {
        body.push_str(&format!("<{}>{}</{}>", k, xml_escape(v), k));
    }
    body.push_str(&format!("</u:{}></s:Body></s:Envelope>", action));
    body
}

pub fn soap_call(
    host: &str,
    port: u16,
    control_path: &str,
    service: &str,
    action: &str,
    args: &[(&str, String)],
    timeout: Duration,
) -> Res<String> {
    let body = soap_body(service, action, args);
    // UPnP/SOAP call against a Sonos control URL; returns the response body.

    let headers: Vec<(&str, String)> = vec![
        (
            "Content-Type",
            String::from("text/xml; charset=\"utf-8\""),
        ),
        // The action MUST be inside the quotes: "<service>#<action>".
        // Sonos dispatches the action anyway but answers UPnPError 402
        // (invalid arguments) for the malformed "<service>"#<action> form.
        ("SOAPAction", format!("\"{}#{}\"", service, action)),
    ];
    let resp = http_request(
        "POST",
        host,
        port,
        control_path,
        &headers,
        Some(body.as_bytes()),
        timeout,
    )?;
    if resp.status != 200 {
        // Report the UPnP error code: it is the only part of a Sonos fault that
        // says what actually went wrong (402 bad args, 701 bad state, 749 URI
        // refused, ...). Without it every failure looks identical.
        let code = crate::xml::tag_value(&resp.body, "errorCode");
        let fault = crate::xml::tag_value(&resp.body, "faultstring")
            .or_else(|| crate::xml::tag_value(&resp.body, "error_description"));
        let msg = match (&code, &fault) {
            (Some(c), Some(f)) if f != "UPnPError" => format!("{} ({})", f, c),
            (Some(c), _) => format!("{} ({})", upnp_error_name(c), c),
            (None, Some(f)) => f.clone(),
            _ => format!("HTTP {}", resp.status),
        };
        return Err(estr(format!("{}: {}", action, msg)));
    }
    Ok(resp.body)
}

// --------------------------------------------------------------------------- //
// serving side
// --------------------------------------------------------------------------- //

pub struct Req {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
}

impl Req {
    /// Header lookup, case-insensitive (names are stored lower-cased).
    pub fn header(&self, name: &str) -> Option<&str> {
        let n = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == n)
            .map(|(_, v)| v.as_str())
    }
}

pub fn read_request<R: BufRead>(r: &mut R) -> Res<Req> {
    let mut line = String::new();
    if r.read_line(&mut line).map_err(estr)? == 0 {
        return Err(estr("empty request"));
    }
    if line.len() > 8192 {
        return Err(estr("request line too long"));
    }
    let trimmed = line.trim_end();
    let mut it = trimmed.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let target = it.next().unwrap_or("/").to_string();
    let (path, query) = match target.find('?') {
        Some(p) => (target[..p].to_string(), target[p + 1..].to_string()),
        None => (target.clone(), String::new()),
    };
    let mut headers = Vec::new();
    let mut total = 0usize;
    for _ in 0..200 {
        let mut h = String::new();
        let n = r.read_line(&mut h).map_err(estr)?;
        total += n;
        if total > 65536 {
            return Err(estr("headers too large"));
        }
        if n == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some(p) = h.find(':') {
            headers.push((
                h[..p].trim().to_ascii_lowercase(),
                h[p + 1..].trim().to_string(),
            ));
        }
    }
    Ok(Req {
        method,
        path,
        query,
        headers,
    })
}

pub fn write_response<W: Write>(
    w: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> Res<()> {
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        content_type,
        body.len(),
        body
    );
    w.write_all(resp.as_bytes()).map_err(estr)?;
    w.flush().map_err(estr)
}

pub fn write_chunk_header<W: Write>(w: &mut W, len: usize) -> Res<()> {
    w.write_all(format!("{:x}\r\n", len).as_bytes()).map_err(estr)
}

pub fn write_chunk<W: Write>(w: &mut W, data: &[u8]) -> Res<()> {
    if data.is_empty() {
        return Ok(());
    }
    w.write_all(format!("{:x}\r\n", data.len()).as_bytes())
        .map_err(estr)?;
    w.write_all(data).map_err(estr)?;
    w.write_all(b"\r\n").map_err(estr)?;
    w.flush().map_err(estr)
}

pub fn write_last_chunk<W: Write>(w: &mut W) {
    let _ = w.write_all(b"0\r\n\r\n");
    let _ = w.flush();
}

/// Start of a chunked 200 response.
pub fn write_stream_head<W: Write>(w: &mut W, content_type: &str) -> Res<()> {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\n\
         Cache-Control: no-cache, no-store\r\nPragma: no-cache\r\n\
         icy-name: sonos-bridge\r\nConnection: close\r\n\r\n",
        content_type
    );
    w.write_all(resp.as_bytes()).map_err(estr)?;
    w.flush().map_err(estr)
}

// --------------------------------------------------------------------------- //
// MPEG / ID3 framing - lets a late joining speaker start on a frame boundary
// --------------------------------------------------------------------------- //

const BR_V1L3: [u32; 15] = [
    0, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640, 704, 768, 896,
];
const BR_V2L3: [u32; 15] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const SR: [[u32; 3]; 3] = [
    [44100, 48000, 32000],     // MPEG1
    [22050, 24000, 16000],     // MPEG2
    [11025, 12000, 8000],      // MPEG2.5
];

/// Length of an MPEG-1/2 layer-III frame starting at `b[0]`, None if not one.
pub fn mpeg_frame_len(b: &[u8]) -> Option<usize> {
    if b.len() < 4 {
        return None;
    }
    if b[0] != 0xFF || (b[1] & 0xE0) != 0xE0 {
        return None;
    }
    let version = (b[1] >> 3) & 0x03; // 3 = MPEG1, 2 = MPEG2, 0 = MPEG2.5, 1 = bad
    if version == 1 {
        return None;
    }
    let layer = (b[1] >> 1) & 0x03; // 1 = layer III
    if layer != 1 {
        return None;
    }
    let br_index = (b[2] >> 4) & 0x0F;
    if br_index == 0 || br_index == 15 {
        return None;
    }
    let sr_index = (b[2] >> 2) & 0x03;
    if sr_index == 3 {
        return None;
    }
    let padding = (b[2] >> 1) & 0x01;
    let vslot = if version == 3 { 0 } else if version == 2 { 1 } else { 2 };
    let bitrate = if version == 3 {
        BR_V1L3[br_index as usize - 1]
    } else {
        BR_V2L3[br_index as usize - 1]
    };
    if bitrate == 0 {
        return None;
    }
    let rate = SR[vslot][sr_index as usize];
    let len = (144_000 * bitrate / rate) as usize + padding as usize;
    if len < 24 {
        return None;
    }
    Some(len)
}

/// Length of an ID3v2 tag header at `b[0]` (so it can be skipped).
pub fn id3_len(b: &[u8]) -> Option<usize> {
    if b.len() < 10 || &b[0..3] != b"ID3" {
        return None;
    }
    let size = ((b[6] as usize & 0x7f) << 21)
        | ((b[7] as usize & 0x7f) << 14)
        | ((b[8] as usize & 0x7f) << 7)
        | (b[9] as usize & 0x7f);
    Some(10 + size)
}

/// Offset of the first plausible frame boundary in `b`.
pub fn first_frame(b: &[u8]) -> Option<usize> {
    let mut i = 0usize;
    while i + 4 <= b.len() {
        if b[i] == 0xFF {
            if mpeg_frame_len(&b[i..]).is_some() {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Wrap `read` into a BufReader helper used by both servers.
pub fn buffered<S: Read>(s: S) -> BufReader<S> {
    BufReader::with_capacity(8192, s)
}
