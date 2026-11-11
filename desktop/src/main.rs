// RadioWizard for Windows. The window shows the RadioWizard page (bundled in dist/); bridge.js gives it the
// same window.AndroidCast bridge as the Android app, backed by the commands below:
//   http_get / probe   web requests without browser limits (the radio directories and Bandcamp block web pages)
//   bandcamp_*         sign in with Bandcamp's own page; its cookies go along on Bandcamp requests
//   soundcloud_*       sign in with SoundCloud's own page; its token goes along on SoundCloud API requests
//   cast_*             finds Google Cast speakers (mDNS) and plays on them with Google's standard player
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpStream},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

fn err<E: ToString>(e: E) -> String {
    e.to_string()
}

fn app_dir() -> PathBuf {
    dirs::config_local_dir().unwrap_or_else(std::env::temp_dir).join("RadioWizard")
}

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36";

/* ---------------- Web requests ---------------- */

fn bandcamp_cookie() -> Option<String> {
    fs::read_to_string(app_dir().join("bandcamp-cookie.txt")).ok().filter(|s| !s.trim().is_empty())
}

fn soundcloud_token() -> Option<String> {
    fs::read_to_string(app_dir().join("soundcloud-token.txt")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn is_bandcamp(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or("").split(['/', ':', '?']).next().unwrap_or("").to_lowercase();
    host == "bandcamp.com" || host.ends_with(".bandcamp.com")
}

fn agent(follow: bool, timeout_ms: u64) -> Result<ureq::Agent, String> {
    let tls = native_tls::TlsConnector::new().map_err(err)?;
    Ok(ureq::AgentBuilder::new()
        .tls_connector(Arc::new(tls))
        .redirects(if follow { 8 } else { 0 })
        .timeout_connect(Duration::from_millis(timeout_ms.min(8000)))
        .timeout(Duration::from_millis(timeout_ms))
        .user_agent(BROWSER_UA)
        .build())
}

/// Sends the request; error statuses (404...) still come back as a response.
fn request(a: &Value, follow: bool, timeout_ms: u64) -> Result<ureq::Response, String> {
    let url = a["url"].as_str().ok_or("no url")?;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("only web links".into());
    }
    let method = a["method"].as_str().unwrap_or("GET");
    let mut req = agent(follow, timeout_ms)?.request(method, url).set("Icy-MetaData", "0");
    if let Some(h) = a["headers"].as_object() {
        for (k, v) in h {
            if k.eq_ignore_ascii_case("X-RW-NoAuth") { continue; }
            if let Some(v) = v.as_str() {
                req = req.set(k, v);
            }
        }
    }
    if is_bandcamp(url) {
        if let Some(c) = bandcamp_cookie() {
            req = req.set("Cookie", c.trim());
        }
    }
    // Signed in to SoundCloud: its API answers as you (private likes and playlists, full Go+ songs).
    // X-RW-NoAuth: ask as nobody (the page retries this way when SoundCloud turns the sign-in down).
    let no_auth = a["headers"].as_object().map_or(false, |h| h.keys().any(|k| k.eq_ignore_ascii_case("X-RW-NoAuth")));
    if url.starts_with("https://api-v2.soundcloud.com/") && !no_auth {
        if let Some(t) = soundcloud_token() {
            req = req.set("Authorization", &format!("OAuth {t}"));
        }
    }
    let res = match a["body"].as_str() {
        Some(b) => req.set("Content-Type", a["type"].as_str().unwrap_or("application/x-www-form-urlencoded")).send_string(b),
        None => req.call(),
    };
    match res {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(_, r)) => Ok(r),
        Err(e) => Err(err(e)),
    }
}

fn read_up_to(r: ureq::Response, max: u64) -> Vec<u8> {
    read_body(r, max).unwrap_or_default()
}

/// The whole answer, or an error if it was cut off on the way (slow server, time ran out), so the page
/// never takes half an answer for all of it.
fn read_body(r: ureq::Response, max: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    r.into_reader().take(max).read_to_end(&mut out).map_err(|e| format!("answer cut off: {e}"))?;
    Ok(out)
}

#[tauri::command]
async fn http_get(args: String) -> Value {
    tauri::async_runtime::spawn_blocking(move || {
        let a: Value = serde_json::from_str(&args).unwrap_or(json!({}));
        let follow = a["follow"].as_bool().unwrap_or(true);
        let timeout = a["timeout"].as_u64().unwrap_or(12000);
        match request(&a, follow, timeout) {
            Ok(r) => {
                let status = r.status();
                let url = r.get_url().to_string();
                let location = r.header("location").map(|s| s.to_string());
                let ctype = r.header("content-type").unwrap_or("").to_string();
                let text = if (300..400).contains(&status) { String::new() } else {
                    match read_body(r, 3_000_000) { Ok(b) => String::from_utf8_lossy(&b).into_owned(), Err(e) => return json!({ "error": e }) }
                };
                json!({ "status": status, "url": url, "location": location, "contentType": ctype, "text": text })
            }
            Err(e) => json!({ "error": e }),
        }
    })
    .await
    .unwrap_or_else(|e| json!({ "error": err(e) }))
}

/// Tests a stream: does it answer and start sending something? Only the first few KB are read.
#[tauri::command]
async fn probe(args: String) -> Value {
    tauri::async_runtime::spawn_blocking(move || {
        let a: Value = serde_json::from_str(&args).unwrap_or(json!({}));
        let start = a["url"].as_str().unwrap_or("").to_string();
        match request(&a, true, a["timeout"].as_u64().unwrap_or(7000)) {
            Ok(r) => {
                let status = r.status();
                let url = r.get_url().to_string();
                let ctype = r.header("content-type").unwrap_or("").to_lowercase();
                let bytes = if (200..300).contains(&status) { read_up_to(r, 4096) } else { Vec::new() };
                let texty = ctype.starts_with("text/") || ctype.contains("mpegurl") || ctype.contains("scpls") || ctype.contains("html");
                let ok = (200..300).contains(&status) && (bytes.len() > 200 || (texty && !bytes.is_empty()));
                let mut out = json!({ "ok": ok, "status": status, "url": url, "contentType": ctype });
                if texty || bytes.iter().take(16).all(|b| (9..=126).contains(b)) {
                    out["head"] = json!(String::from_utf8_lossy(&bytes));
                }
                out
            }
            // Old SHOUTcast servers answer "ICY 200 OK", which isn't HTTP but plays fine.
            Err(e) => {
                let icy = e.contains("ICY") || e.contains("BadStatus") || e.contains("Bad Status");
                json!({ "ok": icy, "url": start, "contentType": if icy { "audio/mpeg" } else { "" }, "error": e })
            }
        }
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": err(e) }))
}

/// The song playing on a station: asks the stream for its titles ("ICY" metadata) and reads the first one.
#[tauri::command]
async fn icy(args: String) -> Value {
    tauri::async_runtime::spawn_blocking(move || {
        let mut a: Value = serde_json::from_str(&args).unwrap_or(json!({}));
        a["headers"] = json!({ "Icy-MetaData": "1" });
        let r = match request(&a, true, a["timeout"].as_u64().unwrap_or(9000)) {
            Ok(r) => r,
            Err(e) => return json!({ "ok": false, "error": e }),
        };
        let every: usize = match r.header("icy-metaint").and_then(|v| v.trim().parse().ok()) {
            Some(n) if n > 0 && n <= 300_000 => n,
            _ => return json!({ "ok": false, "why": "no titles" }),
        };
        let mut rd = r.into_reader();
        let mut skip = vec![0u8; every];
        if rd.read_exact(&mut skip).is_err() {
            return json!({ "ok": false, "why": "stream ended" });
        }
        let mut len = [0u8; 1];
        if rd.read_exact(&mut len).is_err() {
            return json!({ "ok": false });
        }
        let mut meta = vec![0u8; len[0] as usize * 16];
        if rd.read_exact(&mut meta).is_err() {
            return json!({ "ok": false });
        }
        let text = match String::from_utf8(meta.clone()) {
            Ok(t) => t,
            Err(_) => meta.iter().map(|&b| b as char).collect(),   // old stations send Latin-1
        };
        let title = text.split("StreamTitle='").nth(1).and_then(|t| t.split("';").next()).unwrap_or("").trim().to_string();
        json!({ "ok": !title.is_empty(), "title": title })
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": err(e) }))
}

/* ---------------- Bandcamp sign-in ---------------- */

#[tauri::command]
async fn bandcamp_login(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("bclogin") {
        let _ = w.set_focus();
        return Ok(());
    }
    let url: tauri::Url = "https://bandcamp.com/login".parse().map_err(err)?;
    WebviewWindowBuilder::new(&app, "bclogin", WebviewUrl::External(url))
        .title("Sign in to Bandcamp")
        .inner_size(520.0, 760.0)
        .build()
        .map_err(err)?;
    // Watch for Bandcamp's "identity" cookie (set once you're signed in), then keep the cookies and close.
    std::thread::spawn(move || {
        let site: tauri::Url = "https://bandcamp.com/".parse().unwrap();
        let mut ok = false;
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            let Some(w) = app.get_webview_window("bclogin") else { break };
            if let Ok(cookies) = w.cookies_for_url(site.clone()) {
                if cookies.iter().any(|c| c.name() == "identity" && !c.value().is_empty()) {
                    let header = cookies.iter().map(|c| format!("{}={}", c.name(), c.value())).collect::<Vec<_>>().join("; ");
                    let _ = fs::create_dir_all(app_dir());
                    ok = fs::write(app_dir().join("bandcamp-cookie.txt"), header).is_ok();
                    let _ = w.close();
                    break;
                }
            }
        }
        let ok = ok || bandcamp_cookie().is_some();
        if let Some(main) = app.get_webview_window("main") {
            let _ = main.eval(format!("window.onBandcampLogin && window.onBandcampLogin({ok})"));
        }
    });
    Ok(())
}

#[tauri::command]
fn bandcamp_logout() {
    let _ = fs::remove_file(app_dir().join("bandcamp-cookie.txt"));
}

/* ---------------- Backup folder ---------------- */
// The page's backups go to a folder picked here (a real Windows dialog, owned by RW's window), remembered in
// backup-folder.txt. The page reads and writes its .md files through these, like the Android app.

fn backup_dir() -> Option<PathBuf> {
    let p = PathBuf::from(fs::read_to_string(app_dir().join("backup-folder.txt")).ok()?.trim());
    if !p.is_dir() { fs::create_dir_all(&p).ok()?; }   // deleted since it was picked: make it again
    Some(p)
}
fn backup_file(name: &str) -> Option<PathBuf> {
    let ok = !name.is_empty() && name.ends_with(".md") && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_');
    if ok { backup_dir().map(|d| d.join(name)) } else { None }
}

#[tauri::command]
async fn backup_pick(window: tauri::WebviewWindow) -> Option<String> {
    let mut d = rfd::AsyncFileDialog::new().set_title("Pick a folder for RadioWizard backups").set_parent(&window);
    if let Some(p) = backup_dir() { d = d.set_directory(p); }
    let path = d.pick_folder().await?.path().to_string_lossy().to_string();
    let _ = fs::create_dir_all(app_dir());
    let _ = fs::write(app_dir().join("backup-folder.txt"), &path);
    Some(path)
}

#[tauri::command]
fn backup_folder() -> Option<String> {
    backup_dir().map(|p| p.to_string_lossy().to_string())
}

#[tauri::command]
fn backup_read(name: String) -> serde_json::Value {
    match backup_file(&name).and_then(|f| fs::read_to_string(f).ok()) {
        Some(text) => serde_json::json!({ "ok": true, "text": text }),
        None => serde_json::json!({ "ok": false }),
    }
}

#[tauri::command]
fn backup_write(name: String, text: String) -> bool {
    backup_file(&name).map(|f| fs::write(f, text).is_ok()).unwrap_or(false)
}

#[tauri::command]
fn backup_forget() {
    let _ = fs::remove_file(app_dir().join("backup-folder.txt"));
}

/* ---------------- SoundCloud sign-in ---------------- */

#[tauri::command]
async fn soundcloud_login(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("sclogin") {
        let _ = w.set_focus();
        return Ok(());
    }
    let url: tauri::Url = "https://soundcloud.com/signin".parse().map_err(err)?;
    WebviewWindowBuilder::new(&app, "sclogin", WebviewUrl::External(url))
        .title("Sign in to SoundCloud")
        .inner_size(520.0, 760.0)
        .build()
        .map_err(err)?;
    // Watch for SoundCloud's "oauth_token" cookie (set once you're signed in), then keep it and close.
    std::thread::spawn(move || {
        let site: tauri::Url = "https://soundcloud.com/".parse().unwrap();
        let mut ok = false;
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            let Some(w) = app.get_webview_window("sclogin") else { break };
            if let Ok(cookies) = w.cookies_for_url(site.clone()) {
                if let Some(c) = cookies.iter().find(|c| c.name() == "oauth_token" && !c.value().is_empty()) {
                    let _ = fs::create_dir_all(app_dir());
                    ok = fs::write(app_dir().join("soundcloud-token.txt"), c.value()).is_ok();
                    let _ = w.close();
                    break;
                }
            }
        }
        let ok = ok || soundcloud_token().is_some();
        if let Some(main) = app.get_webview_window("main") {
            let _ = main.eval(format!("window.onSoundcloudLogin && window.onSoundcloudLogin({ok})"));
        }
    });
    Ok(())
}

/// Opens a web link in the PC's own browser (the app window ignores links meant for a new window).
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.contains('"') {
        return Err("only web links".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", &url]).creation_flags(0x0800_0000).spawn().map_err(err)?;
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open").arg(&url).spawn().map_err(err)?;
    }
    Ok(())
}

#[tauri::command]
fn soundcloud_logout() {
    let _ = fs::remove_file(app_dir().join("soundcloud-token.txt"));
}

/* ---------------- Finding speakers ---------------- */

#[derive(Clone)]
struct Device {
    name: String,
    model: String,
    addr: SocketAddr,
}

type Devices = Arc<Mutex<HashMap<String, Device>>>;

fn discover(devices: Devices) {
    std::thread::spawn(move || {
        let Ok(mdns) = mdns_sd::ServiceDaemon::new() else { return };
        let Ok(rx) = mdns.browse("_googlecast._tcp.local.") else { return };
        while let Ok(ev) = rx.recv() {
            match ev {
                mdns_sd::ServiceEvent::ServiceResolved(s) => {
                    let Some(ip) = s.get_addresses_v4().into_iter().next() else { continue };
                    let name = s.get_property_val_str("fn").unwrap_or(s.get_fullname()).to_string();
                    let model = s.get_property_val_str("md").unwrap_or("").to_string();
                    devices.lock().unwrap().insert(
                        s.get_fullname().to_string(),
                        Device { name, model, addr: SocketAddr::new(IpAddr::V4(ip), s.get_port()) },
                    );
                }
                mdns_sd::ServiceEvent::ServiceRemoved(_, full) => {
                    devices.lock().unwrap().remove(&full);
                }
                _ => {}
            }
        }
    });
}

/* ---------------- Google Cast ---------------- */
// The Cast protocol: TLS to the speaker (port 8009, or a group's own port), each message a 4-byte length plus a
// small protobuf (source, destination, namespace, JSON text). The app talks to Google's standard player (CC1AD845).

const NS_CONN: &str = "urn:x-cast:com.google.cast.tp.connection";
const NS_BEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
const NS_RECV: &str = "urn:x-cast:com.google.cast.receiver";
const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
const PLAYER_APP: &str = "CC1AD845";

fn varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn encode(src: &str, dst: &str, ns: &str, payload: &str) -> Vec<u8> {
    let mut m = vec![0x08, 0x00];
    for (tag, s) in [(0x12u8, src), (0x1a, dst), (0x22, ns)] {
        m.push(tag);
        varint(s.len() as u64, &mut m);
        m.extend_from_slice(s.as_bytes());
    }
    m.extend_from_slice(&[0x28, 0x00, 0x32]);
    varint(payload.len() as u64, &mut m);
    m.extend_from_slice(payload.as_bytes());
    let mut out = (m.len() as u32).to_be_bytes().to_vec();
    out.extend(m);
    out
}

/// Returns (source, namespace, payload).
fn decode(m: &[u8]) -> Option<(String, String, String)> {
    let (mut i, mut src, mut ns, mut payload) = (0usize, String::new(), String::new(), String::new());
    let read_varint = |i: &mut usize| -> Option<u64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = *m.get(*i)?;
            *i += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
        }
    };
    while i < m.len() {
        let key = read_varint(&mut i)?;
        match key & 7 {
            0 => {
                read_varint(&mut i)?;
            }
            2 => {
                let n = read_varint(&mut i)? as usize;
                let s = String::from_utf8_lossy(m.get(i..i + n)?).into_owned();
                i += n;
                match key >> 3 {
                    2 => src = s,
                    4 => ns = s,
                    6 => payload = s,
                    _ => {}
                }
            }
            _ => return None,
        }
    }
    Some((src, ns, payload))
}

#[derive(Default, Clone)]
struct Status {
    connecting: bool,
    connected: bool,
    device: String,
    player_state: Option<String>,
    idle_reason: Option<String>,
    content_id: Option<String>,
    volume: Option<f64>,
    muted: bool,
    error: Option<String>,
}

enum Cmd {
    Load(Value, mpsc::Sender<Result<(), String>>),
    QueueAdd(Vec<Value>, mpsc::Sender<Result<(), String>>),
    Media(String),
    Volume(Option<f64>, Option<bool>),
    End(bool),
}

struct Cast {
    tx: Option<mpsc::Sender<Cmd>>,
    status: Arc<Mutex<Status>>,
}

struct AppState {
    devices: Devices,
    cast: Mutex<Cast>,
}

fn media_of(a: &Value) -> Value {
    let mut media = json!({
        "contentId": a["url"], "contentType": a["contentType"].as_str().unwrap_or("audio/mpeg"),
        "streamType": if a["streamType"].as_str() == Some("BUFFERED") { "BUFFERED" } else { "LIVE" },
        "metadata": { "metadataType": 0, "title": a["title"], "subtitle": a["subtitle"] },
    });
    if let Some(img) = a["image"].as_str().filter(|s| !s.is_empty()) {
        media["metadata"]["images"] = json!([{ "url": img }]);
    }
    media
}

fn cast_thread(dev: Device, rx: mpsc::Receiver<Cmd>, status: Arc<Mutex<Status>>, ready: mpsc::Sender<Result<(), String>>) {
    let set = |f: &dyn Fn(&mut Status)| f(&mut status.lock().unwrap());
    let fail = |e: String| {
        let mut s = status.lock().unwrap();
        s.connected = false;
        s.connecting = false;
        s.error = Some(e);
    };
    let tcp = match TcpStream::connect_timeout(&dev.addr, Duration::from_secs(6)) {
        Ok(t) => t,
        Err(e) => {
            fail(err(&e));
            let _ = ready.send(Err(err(e)));
            return;
        }
    };
    let _ = tcp.set_nodelay(true);
    // Speakers sign their own certificates.
    let tls = native_tls::TlsConnector::builder().danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true).build();
    let mut s = match tls.map_err(err).and_then(|t| t.connect("cast", tcp).map_err(err)) {
        Ok(s) => s,
        Err(e) => {
            fail(e.clone());
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = s.get_ref().set_read_timeout(Some(Duration::from_millis(150)));

    let mut req_id = 1u64;
    let mut send = |s: &mut native_tls::TlsStream<TcpStream>, dst: &str, ns: &str, mut v: Value| -> std::io::Result<u64> {
        req_id += 1;
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
        if !["PONG", "PING", "CONNECT", "CLOSE"].contains(&ty.as_str()) {
            v["requestId"] = json!(req_id);
        }
        s.write_all(&encode("sender-0", dst, ns, &v.to_string()))?;
        Ok(req_id)
    };

    let _ = send(&mut s, "receiver-0", NS_CONN, json!({ "type": "CONNECT" }));
    let _ = send(&mut s, "receiver-0", NS_RECV, json!({ "type": "GET_STATUS" }));

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16384];
    let (mut transport, mut session, mut media_sid): (Option<String>, Option<String>, Option<u64>) = (None, None, None);
    let mut launched = false;
    let mut ready = Some(ready);
    let mut waiting_load: Option<(u64, mpsc::Sender<Result<(), String>>, Instant)> = None;
    let mut queued_load: Option<(Value, mpsc::Sender<Result<(), String>>)> = None;
    let mut last_ping = Instant::now();
    let mut last_heard = Instant::now();
    let started = Instant::now();

    let end = |why: Option<String>| {
        let mut st = status.lock().unwrap();
        *st = Status { error: why, ..Default::default() };
    };

    loop {
        // Commands from the page
        loop {
            match rx.try_recv() {
                Ok(Cmd::Load(a, reply)) => queued_load = Some((a, reply)),
                Ok(Cmd::QueueAdd(items, reply)) => {
                    // more songs at the end of the speaker's queue (it moves on to them by itself)
                    match (&transport, media_sid) {
                        (Some(t), Some(id)) => {
                            let items: Vec<Value> = items.iter().map(|a| json!({ "media": media_of(a), "autoplay": true, "preloadTime": 15 })).collect();
                            let r = send(&mut s, t, NS_MEDIA, json!({ "type": "QUEUE_INSERT", "mediaSessionId": id, "items": items })).map(|_| ()).map_err(err);
                            let _ = reply.send(r);
                        }
                        _ => {
                            let _ = reply.send(Err("nothing playing".into()));
                        }
                    }
                }
                Ok(Cmd::Media(what)) => {
                    if let (Some(t), Some(id)) = (&transport, media_sid) {
                        let _ = send(&mut s, t, NS_MEDIA, json!({ "type": what, "mediaSessionId": id }));
                    }
                }
                Ok(Cmd::Volume(level, muted)) => {
                    let mut vol = json!({});
                    if let Some(l) = level {
                        vol["level"] = json!(l.clamp(0.0, 1.0));
                    }
                    if let Some(m) = muted {
                        vol["muted"] = json!(m);
                    }
                    let _ = send(&mut s, "receiver-0", NS_RECV, json!({ "type": "SET_VOLUME", "volume": vol }));
                }
                Ok(Cmd::End(stop)) => {
                    if stop {
                        if let Some(sid) = &session {
                            let _ = send(&mut s, "receiver-0", NS_RECV, json!({ "type": "STOP", "sessionId": sid }));
                        }
                    }
                    if let Some(t) = &transport {
                        let _ = send(&mut s, t, NS_CONN, json!({ "type": "CLOSE" }));
                    }
                    end(None);
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    end(None);
                    return;
                }
            }
        }
        if transport.is_some() && queued_load.is_some() {
            let t = transport.clone().unwrap();
            let (a, reply) = queued_load.take().unwrap();
            let media = media_of(&a);
            // Songs go in a speaker queue, so later ones can be added and the speaker moves on by itself.
            let mut msg = if a["queue"].as_bool() == Some(true) {
                json!({ "type": "QUEUE_LOAD", "items": [{ "media": media, "autoplay": true, "preloadTime": 15 }], "startIndex": 0, "repeatMode": "REPEAT_OFF", "sessionId": session })
            } else {
                json!({ "type": "LOAD", "media": media, "autoplay": true, "sessionId": session })
            };
            if let Some(ct) = a["currentTime"].as_f64().filter(|t| *t > 0.0) {
                msg["currentTime"] = json!(ct);
            }
            match send(&mut s, &t, NS_MEDIA, msg) {
                Ok(id) => {
                    set(&|st| st.content_id = a["url"].as_str().map(|u| u.to_string()));
                    waiting_load = Some((id, reply, Instant::now()));
                }
                Err(e) => {
                    let _ = reply.send(Err(err(e)));
                }
            }
        }
        if waiting_load.as_ref().is_some_and(|w| w.2.elapsed() > Duration::from_secs(25)) {
            let (_, reply, _) = waiting_load.take().unwrap();
            let _ = reply.send(Err("the speaker didn't answer".into()));
        }
        if ready.is_some() && started.elapsed() > Duration::from_secs(20) {
            let _ = ready.take().unwrap().send(Err("the speaker didn't start the player".into()));
            end(Some("start timed out".into()));
            return;
        }

        // Keep the connection alive
        if last_ping.elapsed() > Duration::from_secs(5) {
            last_ping = Instant::now();
            if send(&mut s, "receiver-0", NS_BEAT, json!({ "type": "PING" })).is_err() {
                end(Some("connection lost".into()));
                return;
            }
        }
        if last_heard.elapsed() > Duration::from_secs(20) {
            end(Some("speaker stopped answering".into()));
            return;
        }

        // Messages from the speaker
        match s.read(&mut chunk) {
            Ok(0) => {
                end(Some("speaker closed the connection".into()));
                return;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => {
                end(Some(err(e)));
                return;
            }
        }
        while buf.len() >= 4 {
            let n = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
            if buf.len() < 4 + n {
                break;
            }
            let msg: Vec<u8> = buf.drain(..4 + n).skip(4).collect();
            let Some((src, ns, payload)) = decode(&msg) else { continue };
            last_heard = Instant::now();
            let v: Value = serde_json::from_str(&payload).unwrap_or(json!({}));
            let ty = v["type"].as_str().unwrap_or("");
            match (ns.as_str(), ty) {
                (NS_BEAT, "PING") => {
                    let _ = send(&mut s, &src, NS_BEAT, json!({ "type": "PONG" }));
                }
                (NS_RECV, "RECEIVER_STATUS") => {
                    let st = &v["status"];
                    if let Some(l) = st["volume"]["level"].as_f64() {
                        let m = st["volume"]["muted"].as_bool().unwrap_or(false);
                        set(&|x| {
                            x.volume = Some(l);
                            x.muted = m;
                        });
                    }
                    let app = st["applications"].as_array().and_then(|a| a.iter().find(|x| x["appId"] == PLAYER_APP)).cloned();
                    match app {
                        Some(app) => {
                            let t = app["transportId"].as_str().unwrap_or("").to_string();
                            session = app["sessionId"].as_str().map(|x| x.to_string());
                            if transport.as_deref() != Some(&t) && !t.is_empty() {
                                let _ = send(&mut s, &t, NS_CONN, json!({ "type": "CONNECT" }));
                                let _ = send(&mut s, &t, NS_MEDIA, json!({ "type": "GET_STATUS" }));
                                transport = Some(t);
                                let name = dev.name.clone();
                                set(&|x| {
                                    x.connecting = false;
                                    x.connected = true;
                                    x.device = name.clone();
                                    x.error = None;
                                });
                                if let Some(r) = ready.take() {
                                    let _ = r.send(Ok(()));
                                }
                            }
                        }
                        None => {
                            if transport.is_some() {
                                // Someone else took the speaker (or it was stopped from Google Home).
                                end(Some("the speaker's player was closed".into()));
                                return;
                            }
                            if !launched {
                                launched = true;
                                let _ = send(&mut s, "receiver-0", NS_RECV, json!({ "type": "LAUNCH", "appId": PLAYER_APP }));
                            }
                        }
                    }
                }
                (NS_RECV, "LAUNCH_ERROR") => {
                    if let Some(r) = ready.take() {
                        let _ = r.send(Err("the speaker wouldn't start the player".into()));
                    }
                    end(Some("launch error".into()));
                    return;
                }
                (NS_MEDIA, "MEDIA_STATUS") => {
                    if let Some(m) = v["status"].as_array().and_then(|a| a.first()) {
                        media_sid = m["mediaSessionId"].as_u64().or(media_sid);
                        let ps = m["playerState"].as_str().map(|x| x.to_string());
                        let ir = m["idleReason"].as_str().map(|x| x.to_string());
                        let cid = m["media"]["contentId"].as_str().map(|x| x.to_string());
                        set(&|x| {
                            x.player_state = ps.clone();
                            x.idle_reason = ir.clone();
                            if cid.is_some() {
                                x.content_id = cid.clone();
                            }
                        });
                    }
                    if waiting_load.as_ref().is_some_and(|w| v["requestId"].as_u64() == Some(w.0)) {
                        let (_, reply, _) = waiting_load.take().unwrap();
                        let _ = reply.send(Ok(()));
                    }
                }
                (NS_MEDIA, "LOAD_FAILED") | (NS_MEDIA, "LOAD_CANCELLED") | (NS_MEDIA, "INVALID_REQUEST") => {
                    if waiting_load.as_ref().is_some_and(|w| v["requestId"].as_u64() == Some(w.0) || ty == "LOAD_FAILED") {
                        let (_, reply, _) = waiting_load.take().unwrap();
                        let _ = reply.send(Err(ty.to_string()));
                        set(&|x| {
                            x.player_state = Some("IDLE".into());
                            x.idle_reason = Some("ERROR".into());
                        });
                    }
                }
                (NS_CONN, "CLOSE") if Some(&src) == transport.as_ref() => {
                    end(Some("the speaker's player was closed".into()));
                    return;
                }
                _ => {}
            }
        }
    }
}

#[tauri::command]
fn cast_devices(state: tauri::State<AppState>) -> Value {
    let mut list: Vec<Value> = state.devices.lock().unwrap().iter().map(|(id, d)| json!({ "id": id, "name": d.name, "model": d.model })).collect();
    list.sort_by(|a, b| a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase()));
    Value::Array(list)
}

#[tauri::command]
fn cast_state(state: tauri::State<AppState>) -> Value {
    let devices = state.devices.lock().unwrap().len();
    let s = state.cast.lock().unwrap().status.lock().unwrap().clone();
    json!({
        "devices": devices, "connecting": s.connecting, "connected": s.connected, "device": s.device,
        "playerState": s.player_state, "idleReason": s.idle_reason, "contentId": s.content_id,
        "volume": s.volume, "muted": s.muted, "error": s.error,
    })
}

#[tauri::command]
async fn cast_connect(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let dev = state.devices.lock().unwrap().get(&id).cloned().ok_or("that speaker isn't on the network any more")?;
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let status = Arc::new(Mutex::new(Status { connecting: true, device: dev.name.clone(), ..Default::default() }));
    {
        let mut c = state.cast.lock().unwrap();
        if let Some(old) = c.tx.take() {
            let _ = old.send(Cmd::End(false));
        }
        c.tx = Some(tx);
        c.status = status.clone();
    }
    std::thread::spawn(move || cast_thread(dev, rx, status, ready_tx));
    tauri::async_runtime::spawn_blocking(move || ready_rx.recv_timeout(Duration::from_secs(25)).unwrap_or(Err("timed out".into())))
        .await
        .map_err(err)?
}

fn send_cmd(state: &AppState, cmd: Cmd) -> Result<(), String> {
    let c = state.cast.lock().unwrap();
    c.tx.as_ref().ok_or("no speaker connected")?.send(cmd).map_err(|_| "no speaker connected".to_string())
}

#[tauri::command]
async fn cast_load(state: tauri::State<'_, AppState>, args: String) -> Result<(), String> {
    let a: Value = serde_json::from_str(&args).map_err(err)?;
    let (tx, rx) = mpsc::channel();
    send_cmd(&state, Cmd::Load(a, tx))?;
    tauri::async_runtime::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(30)).unwrap_or(Err("timed out".into())))
        .await
        .map_err(err)?
}

#[tauri::command]
async fn cast_queue_add(state: tauri::State<'_, AppState>, args: String) -> Result<(), String> {
    let a: Value = serde_json::from_str(&args).map_err(err)?;
    let items = a["items"].as_array().cloned().unwrap_or_default();
    let (tx, rx) = mpsc::channel();
    send_cmd(&state, Cmd::QueueAdd(items, tx))?;
    tauri::async_runtime::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(15)).unwrap_or(Err("timed out".into())))
        .await
        .map_err(err)?
}

#[tauri::command]
fn cast_media(state: tauri::State<AppState>, what: String) -> Result<(), String> {
    if !["PAUSE", "PLAY", "STOP"].contains(&what.as_str()) {
        return Err("unknown".into());
    }
    send_cmd(&state, Cmd::Media(what))
}

#[tauri::command]
fn cast_volume(state: tauri::State<AppState>, level: Option<f64>, muted: Option<bool>) -> Result<(), String> {
    send_cmd(&state, Cmd::Volume(level, muted))
}

#[tauri::command]
fn cast_end(state: tauri::State<AppState>, stop: bool) -> Result<(), String> {
    let mut c = state.cast.lock().unwrap();
    if let Some(tx) = c.tx.take() {
        let _ = tx.send(Cmd::End(stop));
    }
    *c.status.lock().unwrap() = Status::default();
    Ok(())
}

fn main() {
    let devices: Devices = Arc::new(Mutex::new(HashMap::new()));
    discover(devices.clone());
    tauri::Builder::default()
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(AppState { devices, cast: Mutex::new(Cast { tx: None, status: Arc::new(Mutex::new(Status::default())) }) })
        .invoke_handler(tauri::generate_handler![
            http_get, probe, icy, open_url, bandcamp_login, bandcamp_logout, soundcloud_login, soundcloud_logout, backup_pick, backup_folder, backup_read, backup_write, backup_forget, cast_devices, cast_state, cast_connect, cast_load, cast_queue_add, cast_media, cast_volume, cast_end
        ])
        .setup(|app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("RadioWizard")
                .inner_size(820.0, 900.0)
                .min_inner_size(360.0, 480.0)
                .initialization_script(include_str!("bridge.js"))
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running RadioWizard");
}
