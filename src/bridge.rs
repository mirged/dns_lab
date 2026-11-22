//! JNI surface for the Java Android UI.
//!
//! The Android app is a plain `Activity` (see `android/src/`) with no AndroidX
//! and no Gradle: it is compiled by cargo-apk2 with `javac` against
//! `android.jar`. Everything here is therefore a thin, boring adapter over the
//! same `Node`/`Db` the desktop GUI uses - no protocol logic lives in the
//! bridge, so a bug here cannot corrupt the wire format.
//!
//! Data crosses the boundary as one string of records: fields are joined with
//! `\u{1f}` (unit separator) and records with `\n`. Values have their own
//! `\u{1f}` and `\n` replaced by `\u{1e}`, so the Java side can `split()` with
//! no JSON parser. Polling is the Java side's job (`Handler` every ~500 ms):
//! JNI callbacks from arbitrary Rust threads need a JVM attach dance and buy
//! nothing for a UI that repaints on a timer anyway.

use crate::config::{Config, EgressKind};
use crate::db::Db;
use crate::net::{AppEvent, Node};
use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jint, jlong, jstring};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const SEP: char = '\u{1f}';
const NL: char = '\n';
const ESC: char = '\u{1e}';

struct State {
    node: Arc<Node>,
    db: Arc<Db>,
    cfg: Config,
}

struct Session {
    path: String,
    logs: Arc<Mutex<VecDeque<String>>>,
    state: Mutex<State>,
    #[cfg(feature = "audio")]
    rec: Mutex<Option<crate::voice::audio::Recorder>>,
    #[cfg(feature = "audio")]
    play: Mutex<Option<crate::voice::audio::Listen>>,
}

static SESSIONS: OnceLock<Mutex<HashMap<i64, Arc<Session>>>> = OnceLock::new();
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

fn sessions() -> &'static Mutex<HashMap<i64, Arc<Session>>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn session(handle: jlong) -> Option<Arc<Session>> {
    sessions().lock().ok()?.get(&handle).cloned()
}

fn clean(v: &str) -> String {
    v.replace([SEP, NL], "\u{1e}")
}

fn record(fields: &[&str]) -> String {
    fields
        .iter()
        .map(|f| clean(f))
        .collect::<Vec<_>>()
        .join(&SEP.to_string())
}

fn unescape(v: &str) -> String {
    v.replace(ESC, "\n")
}

fn get(env: &mut JNIEnv, s: &JString) -> Option<String> {
    env.get_string(s)
        .ok()
        .map(|s| s.to_string_lossy().into_owned())
}

fn put(env: &mut JNIEnv, s: &str) -> jstring {
    match env.new_string(s) {
        Ok(j) => j.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn drain_events(rx: std::sync::mpsc::Receiver<AppEvent>, sink: Arc<Mutex<VecDeque<String>>>) {
    std::thread::spawn(move || {
        while let Ok(ev) = rx.recv() {
            if let AppEvent::Log(line) = ev
                && let Ok(mut q) = sink.lock()
            {
                q.push_back(line);
                if q.len() > 500 {
                    q.pop_front();
                }
            }
        }
    });
}

fn build_state(
    cfg: Config,
    db: Arc<Db>,
    logs: &Arc<Mutex<VecDeque<String>>>,
) -> Result<State, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    drain_events(rx, logs.clone());
    let node = Node::new(cfg.clone(), db.clone(), tx)?;
    node.start();
    Ok(State { node, db, cfg })
}

#[cfg(feature = "audio")]
fn start_playback(session: &Session, state: &State) {
    match crate::voice::audio::listen(&state.node, &state.cfg, None) {
        Ok(listen) => {
            if let Ok(mut slot) = session.play.lock() {
                *slot = Some(listen);
            }
        }
        Err(e) => {
            state.node.log(format!("[~] voice playback disabled: {e}"));
        }
    }
}

#[cfg(not(feature = "audio"))]
fn start_playback(_session: &Session, _state: &State) {}

/// `Native.init(Activity)`: give cpal's AAudio backend the JVM and the
/// application context. android-activity used to do this for us; with a plain
/// Activity the app has to hand it over explicitly or the first audio call
/// panics inside `ndk_context::android_context()`.
#[unsafe(no_mangle)]
#[allow(unused_mut, unused_variables)]
pub extern "system" fn Java_com_dnslab_messenger_Native_init(
    mut env: JNIEnv,
    _class: JClass,
    activity: JObject,
) -> jboolean {
    #[cfg(target_os = "android")]
    {
        let vm = match env.get_java_vm() {
            Ok(vm) => vm,
            Err(_) => return JNI_FALSE,
        };
        let ctx = match env.new_global_ref(&activity) {
            Ok(ctx) => ctx,
            Err(_) => return JNI_FALSE,
        };
        let vm_ptr = vm.get_java_vm_pointer().cast::<std::ffi::c_void>();
        let ctx_ptr = ctx.as_obj().as_raw().cast::<std::ffi::c_void>();
        unsafe { ndk_context::initialize_android_context(vm_ptr, ctx_ptr) };
        // The global ref has to outlive this call for the lifetime of the app.
        std::mem::forget(ctx);
        JNI_TRUE
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (&mut env, activity);
        JNI_TRUE
    }
}

/// Open the node: config (created on first run), database, listeners, pollers.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_start(
    mut env: JNIEnv,
    _class: JClass,
    config_path: JString,
) -> jlong {
    let Some(path) = get(&mut env, &config_path) else {
        return 0;
    };
    // A fresh install gets a phone-shaped default: the same port the LAN recipe
    // uses, direct egress, and a name that says which device this is. The key
    // still has to be typed in (there is no safe default), so the UI opens on an
    // empty contact list and a "no shared key" hint.
    let existed = std::path::Path::new(&path).exists();
    let mut cfg = Config::load(&path);
    if !existed {
        cfg.display_name = "android".to_string();
        cfg.listen_addr = "0.0.0.0:5354".to_string();
        cfg.needs_write = true;
    }
    if let Some(dir) = std::path::Path::new(&cfg.db_path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::create_dir_all(&cfg.download_dir);
    if cfg.needs_write {
        let _ = cfg.save(&path);
    }
    let db = match Db::open(&cfg.db_path) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            eprintln!("[!] bridge: cannot open {}: {e}", cfg.db_path);
            return 0;
        }
    };
    let logs: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let state = match build_state(cfg, db, &logs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[!] bridge: cannot start node: {e}");
            return 0;
        }
    };
    let session = Arc::new(Session {
        path,
        logs,
        state: Mutex::new(state),
        #[cfg(feature = "audio")]
        rec: Mutex::new(None),
        #[cfg(feature = "audio")]
        play: Mutex::new(None),
    });
    if let Ok(guard) = session.state.lock() {
        start_playback(&session, &guard);
    }
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut map) = sessions().lock() {
        map.insert(handle, session);
    }
    handle
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_stop(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    let session = sessions().lock().ok().and_then(|mut m| m.remove(&handle));
    if let Some(session) = session {
        #[cfg(feature = "audio")]
        {
            if let Ok(mut r) = session.rec.lock()
                && let Some(rec) = r.take()
            {
                rec.stop();
            }
            if let Ok(mut p) = session.play.lock() {
                p.take();
            }
        }
        if let Ok(state) = session.state.lock() {
            state.node.stop();
        }
    }
}

/// node id, routing tag, status, talking, playing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_snapshot(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let Some(session) = session(handle) else {
        return put(&mut env, "");
    };
    let Ok(state) = session.state.lock() else {
        return put(&mut env, "");
    };
    let tag = state
        .node
        .tag
        .clone()
        .unwrap_or_else(|| "(no key)".to_string());
    #[cfg(feature = "audio")]
    let talking = session.rec.lock().map(|r| r.is_some()).unwrap_or(false);
    #[cfg(not(feature = "audio"))]
    let talking = false;
    #[cfg(feature = "audio")]
    let playing = session
        .play
        .lock()
        .ok()
        .and_then(|p| p.as_ref().map(|l| l.playing()))
        .unwrap_or(false);
    #[cfg(not(feature = "audio"))]
    let playing = false;
    let rec = record(&[
        &state.cfg.node_id,
        &tag,
        "online",
        if talking { "1" } else { "0" },
        if playing { "1" } else { "0" },
        &state.cfg.display_name,
    ]);
    drop(state);
    put(&mut env, &rec)
}

/// id, name, last body, last timestamp, addr, via - one record per contact.
///
/// Contacts come from the peer table, not from the message table: a contact
/// with no history must still be listed (and is, in fact, the common case right
/// after it is added).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_contacts(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let Some(session) = session(handle) else {
        return put(&mut env, "");
    };
    let Ok(state) = session.state.lock() else {
        return put(&mut env, "");
    };
    let mut rows: Vec<(String, String, String, i64, String, String)> = state
        .db
        .list_peers()
        .into_iter()
        .map(|p| {
            let name = if p.name.is_empty() {
                p.id.clone()
            } else {
                p.name
            };
            (p.id, name, String::new(), 0, p.addr, p.via)
        })
        .collect();
    for c in state.db.conversations() {
        match rows.iter_mut().find(|r| r.0 == c.peer_id) {
            Some(row) => {
                row.2 = c.last_body;
                row.3 = c.last_ts;
            }
            None => rows.push((
                c.peer_id.clone(),
                if c.peer_name.is_empty() {
                    c.peer_id.clone()
                } else {
                    c.peer_name
                },
                c.last_body,
                c.last_ts,
                String::new(),
                String::new(),
            )),
        }
    }
    let mut out = String::new();
    for (id, name, last, ts, addr, via) in rows {
        out.push_str(&record(&[&id, &name, &last, &ts.to_string(), &addr, &via]));
        out.push(NL);
    }
    drop(state);
    put(&mut env, &out)
}

/// id, direction, status, timestamp, body - newest last.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_messages(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    peer: JString,
    limit: jint,
) -> jstring {
    let peer = get(&mut env, &peer).unwrap_or_default();
    let Some(session) = session(handle) else {
        return put(&mut env, "");
    };
    let Ok(state) = session.state.lock() else {
        return put(&mut env, "");
    };
    let mut out = String::new();
    for m in state.db.messages_for(&peer, limit.max(1) as i64) {
        let status = if m.direction == "out" {
            m.status.as_str()
        } else {
            "received"
        };
        out.push_str(&record(&[
            &m.id.to_string(),
            &m.direction,
            status,
            &m.created_at.to_string(),
            &m.body,
        ]));
        out.push(NL);
    }
    drop(state);
    put(&mut env, &out)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_send(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    to: JString,
    body: JString,
) -> jboolean {
    let to = get(&mut env, &to).unwrap_or_default();
    let body = get(&mut env, &body).unwrap_or_default();
    if to.trim().is_empty() || body.trim().is_empty() {
        return JNI_FALSE;
    }
    let Some(session) = session(handle) else {
        return JNI_FALSE;
    };
    let Ok(state) = session.state.lock() else {
        return JNI_FALSE;
    };
    state.node.send_message(&to, &body);
    JNI_TRUE
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_addContact(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    id: JString,
    name: JString,
    addr: JString,
    via: JString,
) -> jboolean {
    let id = get(&mut env, &id).unwrap_or_default().trim().to_string();
    let name = get(&mut env, &name).unwrap_or_default().trim().to_string();
    let addr = get(&mut env, &addr).unwrap_or_default().trim().to_string();
    let via = get(&mut env, &via).unwrap_or_default().trim().to_string();
    if id.is_empty() {
        return JNI_FALSE;
    }
    let Some(session) = session(handle) else {
        return JNI_FALSE;
    };
    let Ok(mut state) = session.state.lock() else {
        return JNI_FALSE;
    };
    let name = if name.is_empty() { id.clone() } else { name };
    let via = if via.is_empty() {
        "relay".to_string()
    } else {
        via
    };
    state.cfg.peers.retain(|p| p.id != id);
    state.cfg.peers.push(crate::config::Peer {
        id: id.clone(),
        name: name.clone(),
        addr: addr.clone(),
        via: via.clone(),
    });
    let _ = state.cfg.save(&session.path);
    state.db.upsert_peer(&id, &name, &addr, &via);
    JNI_TRUE
}

/// Editable config, one record:
/// display_name, node_id, zone, listen_addr, egress, resolver,
/// direct_server, relay, shared_key, voice_mode, voice_frames, voice_buffer,
/// db_path, download_dir
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_configGet(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let Some(session) = session(handle) else {
        return put(&mut env, "");
    };
    let Ok(state) = session.state.lock() else {
        return put(&mut env, "");
    };
    let cfg = &state.cfg;
    let rec = record(&[
        &cfg.display_name,
        &cfg.node_id,
        &cfg.zone,
        &cfg.listen_addr,
        cfg.egress.as_str(),
        cfg.resolver.as_deref().unwrap_or(""),
        cfg.direct_server.as_deref().unwrap_or(""),
        cfg.relay.as_deref().unwrap_or(""),
        &cfg.shared_key,
        &cfg.voice_mode.to_string(),
        &cfg.voice_frames_per_packet.to_string(),
        &cfg.voice_buffer_ms.to_string(),
        &cfg.db_path,
        &cfg.download_dir,
    ]);
    drop(state);
    put(&mut env, &rec)
}

/// Apply edited fields and restart the node on the new transport settings.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_configSet(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    data: JString,
) -> jboolean {
    let data = get(&mut env, &data).unwrap_or_default();
    let f: Vec<String> = data.split(SEP).map(unescape).collect();
    if f.len() < 12 {
        return JNI_FALSE;
    }
    let Some(session) = session(handle) else {
        return JNI_FALSE;
    };
    let Ok(mut state) = session.state.lock() else {
        return JNI_FALSE;
    };
    // Stop the old node before rebinding the same port.
    state.node.stop();
    {
        let cfg = &mut state.cfg;
        cfg.display_name = f[0].clone();
        cfg.node_id = f[1].clone();
        cfg.zone = f[2].clone();
        cfg.listen_addr = f[3].clone();
        cfg.egress = if f[4] == "resolver" {
            EgressKind::Resolver
        } else {
            EgressKind::Direct
        };
        cfg.resolver = non_empty(&f[5]);
        cfg.direct_server = non_empty(&f[6]);
        cfg.relay = non_empty(&f[7]);
        cfg.shared_key = f[8].clone();
        if let Ok(v) = f[9].parse() {
            cfg.voice_mode = v;
        }
        if let Ok(v) = f[10].parse() {
            cfg.voice_frames_per_packet = v;
        }
        if let Ok(v) = f[11].parse() {
            cfg.voice_buffer_ms = v;
        }
        let _ = cfg.save(&session.path);
    }
    let cfg = state.cfg.clone();
    let db = state.db.clone();
    drop(state);
    // Fresh node, same database: identity and history survive, transport does
    // not (that is the point of restarting).
    let logs = session.logs.clone();
    match build_state(cfg, db, &logs) {
        Ok(new_state) => {
            let mut guard = session.state.lock().unwrap();
            *guard = new_state;
            start_playback(&session, &guard);
            JNI_TRUE
        }
        Err(e) => {
            eprintln!("[!] bridge: restart failed: {e}");
            JNI_FALSE
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_voiceStart(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    peer: JString,
) -> jboolean {
    let peer = get(&mut env, &peer).unwrap_or_default();
    let Some(session) = session(handle) else {
        return JNI_FALSE;
    };
    #[cfg(feature = "audio")]
    {
        let Ok(state) = session.state.lock() else {
            return JNI_FALSE;
        };
        let mode = crate::voice::Mode::from_bitrate(state.cfg.voice_mode);
        let frames = state.cfg.voice_frames_per_packet;
        match crate::voice::audio::Recorder::start(state.node.clone(), peer, mode, frames) {
            Ok(rec) => {
                if let Ok(mut slot) = session.rec.lock() {
                    *slot = Some(rec);
                }
                JNI_TRUE
            }
            Err(e) => {
                state.node.log(format!("[!] microphone unavailable: {e}"));
                JNI_FALSE
            }
        }
    }
    #[cfg(not(feature = "audio"))]
    {
        let _ = (session, peer);
        JNI_FALSE
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_voiceStop(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jboolean {
    let Some(session) = session(handle) else {
        return JNI_FALSE;
    };
    #[cfg(feature = "audio")]
    {
        if let Ok(mut slot) = session.rec.lock()
            && let Some(rec) = slot.take()
        {
            rec.stop();
            return JNI_TRUE;
        }
    }
    JNI_FALSE
}

/// Last `n` log lines, oldest first.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_logTail(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    n: jint,
) -> jstring {
    let Some(session) = session(handle) else {
        return put(&mut env, "");
    };
    let Ok(q) = session.logs.lock() else {
        return put(&mut env, "");
    };
    let take = (n.max(1) as usize).min(q.len());
    let tail: Vec<&String> = q.iter().skip(q.len() - take).collect();
    let body = tail
        .iter()
        .map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    drop(q);
    put(&mut env, &body)
}

/// Absolute path to store config/db under, given a files dir from Java.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_dnslab_messenger_Native_prepare(
    mut env: JNIEnv,
    _class: JClass,
    files_dir: JString,
) -> jstring {
    let base = get(&mut env, &files_dir).unwrap_or_default();
    let dir = std::path::Path::new(&base).join("dns_lab");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("dns_lab.toml").to_string_lossy().into_owned();
    put(&mut env, &path)
}
