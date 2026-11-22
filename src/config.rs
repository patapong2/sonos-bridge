//! Configuration: `key = value` file + environment + CLI overrides, and the
//! well-known runtime paths (control socket, stream token, port file).

use std::fs;
use std::io::Write;

use crate::util::{estr, Res};

pub const APP: &str = "sonos-bridge";

pub const CAPTURE_BACKENDS: [&str; 5] = ["auto", "parec", "ffmpeg", "pw-record", "alsa"];
pub const LOCAL_BACKENDS: [&str; 4] = ["auto", "loopback", "pipe", "none"];
pub const ROUTES: [&str; 4] = ["off", "sonos", "local", "both"];

pub fn config_dir() -> String {
    let base = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| String::from("~/.config"));
    let base = if base.is_empty() {
        String::from("~/.config")
    } else {
        base
    };
    format!("{}/{}", expand_home(&base), APP)
}

pub fn expand_home(p: &str) -> String {
    if p == "~" || p.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp"));
        return format!("{}{}", home, &p[1..]);
    }
    p.to_string()
}

pub fn config_path() -> String {
    std::env::var("SONOS_BRIDGE_CONFIG").unwrap_or_else(|_| format!("{}/config", config_dir()))
}

pub fn runtime_dir() -> String {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| String::from("/tmp"));
    let base = if base.is_empty() {
        String::from("/tmp")
    } else {
        base
    };
    let path = format!("{}/{}", base, APP);
    let _ = fs::create_dir_all(&path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
    }
    path
}

pub fn socket_path() -> String {
    std::env::var("SONOS_BRIDGE_SOCKET").unwrap_or_else(|_| format!("{}/ctl.sock", runtime_dir()))
}

pub fn token_path() -> String {
    std::env::var("SONOS_BRIDGE_TOKEN").unwrap_or_else(|_| format!("{}/stream.token", runtime_dir()))
}

pub fn port_file_path() -> String {
    format!("{}/stream_port", runtime_dir())
}

pub fn log_path() -> String {
    std::env::var("SONOS_BRIDGE_LOG").unwrap_or_else(|_| format!("{}/bridge.log", runtime_dir()))
}

/// Shared secret in the Sonos stream URL: the speakers must be able to pull
/// the stream, other hosts on the LAN should not.
pub fn read_or_make_token() -> String {
    let path = token_path();
    if let Ok(text) = fs::read_to_string(&path) {
        let tok = text.trim().to_string();
        if !tok.is_empty() {
            return tok;
        }
    }
    let mut bytes = [0u8; 16];
    let mut strong = false;
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        use std::io::Read;
        strong = f.read_exact(&mut bytes).is_ok();
    }
    if !strong {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut x = nanos ^ ((std::process::id() as u64) << 17);
        for b in bytes.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = (x & 0xff) as u8;
        }
    }
    let tok: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    if let Ok(mut fh) = fs::File::create(&path) {
        let _ = fh.write_all(tok.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    }
    tok
}

#[derive(Clone, Debug)]
pub struct Config {
    // virtual sink
    pub sink_name: String,
    pub sink_description: String,
    pub capture_backend: String,
    pub alsa_capture: String,

    // capture format
    pub rate: u32,
    pub channels: u32,

    // format handed to the encoder (Sonos friendly)
    pub stream_rate: u32,
    pub stream_channels: u32,

    // sonos stream
    pub stream_format: String,
    pub stream_bitrate: String,
    pub http_host: String,
    pub http_port: u16,
    pub http_bind: String,
    pub ffmpeg: String,

    // local (passthrough) path
    pub local_backend: String,

    // routing
    pub player: String,
    pub route: String,
    pub default_capture_only: bool,

    // control plane
    pub ui_host: String,
    pub ui_port: u16,
    pub verbose: usize,

    /// not persisted - read from runtime dir at startup
    pub stream_token: String,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            sink_name: "sonos_bridge".to_string(),
            sink_description: "Sonos Bridge".to_string(),
            capture_backend: "auto".to_string(),
            alsa_capture: String::new(),
            rate: 48000,
            channels: 2,
            stream_rate: 44100,
            stream_channels: 2,
            stream_format: "mp3".to_string(),
            stream_bitrate: "192k".to_string(),
            http_host: "0.0.0.0".to_string(),
            http_port: 0,
            http_bind: String::new(),
            ffmpeg: "ffmpeg".to_string(),
            local_backend: "auto".to_string(),
            player: String::new(),
            route: "off".to_string(),
            default_capture_only: true,
            ui_host: "127.0.0.1".to_string(),
            ui_port: 8688,
            verbose: 1,
            stream_token: String::new(),
        }
    }
}

impl Config {
    pub fn monitor_source(&self) -> String {
        format!("{}.monitor", self.sink_name)
    }

    pub fn validate(&mut self) -> Res<()> {
        let cleaned: String = self
            .sink_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.sink_name = cleaned;
        if self.sink_name.is_empty() {
            self.sink_name = "sonos_bridge".to_string();
        }
        if self.sink_name.ends_with(".monitor") {
            self.sink_name = self.sink_name[..self.sink_name.len() - 8].to_string();
        }
        if !CAPTURE_BACKENDS.contains(&self.capture_backend.as_str()) {
            return Err(estr(format!(
                "capture_backend must be one of {:?}",
                CAPTURE_BACKENDS
            )));
        }
        if !LOCAL_BACKENDS.contains(&self.local_backend.as_str()) {
            return Err(estr(format!(
                "local_backend must be one of {:?}",
                LOCAL_BACKENDS
            )));
        }
        if !ROUTES.contains(&self.route.as_str()) {
            return Err(estr(format!("route must be one of {:?}", ROUTES)));
        }
        if self.stream_format != "mp3" && self.stream_format != "flac" {
            return Err(estr("stream_format must be mp3 or flac"));
        }
        let br = self.stream_bitrate.replace("k", "");
        let br: u32 = br.parse().map_err(|_| estr("stream_bitrate must look like 128k/192k/320k"))?;
        if !(32..=320).contains(&br) {
            return Err(estr("stream_bitrate out of range (32k..320k)"));
        }
        if !(8000..=192000).contains(&self.rate) {
            return Err(estr("rate out of range"));
        }
        self.channels = if self.channels == 1 { 1 } else { 2 };
        self.stream_channels = if self.stream_channels == 1 { 1 } else { 2 };
        Ok(())
    }

    /// keys that may live in the config file
    pub fn set(&mut self, key: &str, value: &str) -> Res<bool> {
        let v = value.trim().to_string();
        match key {
            "sink_name" => self.sink_name = v,
            "sink_description" => self.sink_description = v,
            "capture_backend" => self.capture_backend = v.to_ascii_lowercase(),
            "alsa_capture" => self.alsa_capture = v,
            "rate" => self.rate = v.parse().map_err(|_| estr("rate must be a number"))?,
            "channels" => self.channels = v.parse().map_err(|_| estr("channels must be 1 or 2"))?,
            "stream_rate" => {
                self.stream_rate = v.parse().map_err(|_| estr("stream_rate must be a number"))?
            }
            "stream_channels" => {
                self.stream_channels = v
                    .parse()
                    .map_err(|_| estr("stream_channels must be 1 or 2"))?
            }
            "stream_format" => self.stream_format = v.to_ascii_lowercase(),
            "stream_bitrate" => self.stream_bitrate = v.to_ascii_lowercase(),
            "http_host" => self.http_host = v,
            "http_port" => self.http_port = v.parse().map_err(|_| estr("http_port must be a number"))?,
            "http_bind" => self.http_bind = v,
            "ffmpeg" => self.ffmpeg = v,
            "local_backend" => self.local_backend = v.to_ascii_lowercase(),
            "player" => self.player = v,
            "route" => self.route = v.to_ascii_lowercase(),
            "default_capture_only" => self.default_capture_only = crate::util::truthy(&v),
            "ui_host" => self.ui_host = v,
            "ui_port" => self.ui_port = v.parse().map_err(|_| estr("ui_port must be a number"))?,
            "verbose" => self.verbose = v.parse().map_err(|_| estr("verbose must be 0..3"))?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn load() -> Res<Config> {
        let mut cfg = Config::default();
        let path = config_path();
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some(pos) = line.find('=') {
                    let key = line[..pos].trim().to_ascii_lowercase();
                    let value = line[pos + 1..].trim().to_string();
                    if !cfg.set(&key, &value)? {
                        crate::log::info("config", &format!("unknown key '{}' in {}", key, path));
                    }
                }
            }
        }
        for (key, var) in [
            ("sink_name", "SONOS_BRIDGE_SINK"),
            ("route", "SONOS_BRIDGE_ROUTE"),
            ("player", "SONOS_BRIDGE_PLAYER"),
            ("capture_backend", "SONOS_BRIDGE_BACKEND"),
            ("stream_format", "SONOS_BRIDGE_FORMAT"),
            ("ui_port", "SONOS_BRIDGE_UI_PORT"),
            ("http_port", "SONOS_BRIDGE_HTTP_PORT"),
            ("ffmpeg", "SONOS_BRIDGE_FFMPEG"),
            ("local_backend", "SONOS_BRIDGE_LOCAL_BACKEND"),
        ]
        .iter()
        {
            if let Ok(v) = std::env::var(var) {
                cfg.set(key, &v)?;
            }
        }
        if let Ok(v) = std::env::var("SONOS_BRIDGE_VERBOSE") {
            cfg.verbose = v.parse().unwrap_or(2);
        }
        cfg.stream_token = read_or_make_token();
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self) -> String {
        let dir = config_dir();
        let _ = fs::create_dir_all(&dir);
        let path = config_path();
        let tmp = format!("{}.tmp", path);
        let body = format!(
            "# sonos-bridge configuration\n\
             sink_name = {sink_name}\n\
             sink_description = {sink_description}\n\
             capture_backend = {capture_backend}\n\
             alsa_capture = {alsa_capture}\n\
             rate = {rate}\n\
             channels = {channels}\n\
             stream_rate = {stream_rate}\n\
             stream_channels = {stream_channels}\n\
             stream_format = {stream_format}\n\
             stream_bitrate = {stream_bitrate}\n\
             http_host = {http_host}\n\
             http_port = {http_port}\n\
             http_bind = {http_bind}\n\
             ffmpeg = {ffmpeg}\n\
             local_backend = {local_backend}\n\
             player = {player}\n\
             route = {route}\n\
             default_capture_only = {default_capture_only}\n\
             ui_host = {ui_host}\n\
             ui_port = {ui_port}\n\
             verbose = {verbose}\n",
            sink_name = self.sink_name,
            sink_description = self.sink_description,
            capture_backend = self.capture_backend,
            alsa_capture = self.alsa_capture,
            rate = self.rate,
            channels = self.channels,
            stream_rate = self.stream_rate,
            stream_channels = self.stream_channels,
            stream_format = self.stream_format,
            stream_bitrate = self.stream_bitrate,
            http_host = self.http_host,
            http_port = self.http_port,
            http_bind = self.http_bind,
            ffmpeg = self.ffmpeg,
            local_backend = self.local_backend,
            player = self.player,
            route = self.route,
            default_capture_only = if self.default_capture_only {
                "true"
            } else {
                "false"
            },
            ui_host = self.ui_host,
            ui_port = self.ui_port,
            verbose = self.verbose,
        );
        let mut out = String::from(body);
        if let Ok(cur) = fs::read_to_string(&path) {
            for line in cur.lines() {
                let l = line.trim();
                if l.is_empty() || l.starts_with('#') {
                    continue;
                }
                if let Some(pos) = l.find('=') {
                    let key = l[..pos].trim();
                    if !out.contains(&format!("\n{} =", key)) && !out.starts_with(&format!("{} =", key))
                    {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
        }
        if let Ok(mut fh) = fs::File::create(&tmp) {
            let _ = fh.write_all(out.as_bytes());
            let _ = fh.flush();
            let _ = fs::rename(&tmp, &path);
        }
        path
    }
}
