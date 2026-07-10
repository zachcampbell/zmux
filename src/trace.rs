// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Best-effort, whole-session diagnostic tracing.
//!
//! The hot path never performs file I/O: records are offered to a bounded
//! channel and are dropped (with an eventual `Gap` record) when the writer
//! cannot keep up.  Trace failures are reflected in [`TraceHub::status`] and
//! never escape into the terminal session.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::state_paths::{ensure_private_dir, safe_component, state_dir};

const MAGIC: &[u8; 8] = b"ZMXTRACE";
const SCHEMA_VERSION: u16 = 1;
const HEADER_LEN: u64 = MAGIC.len() as u64 + 2;
const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_QUEUE_CAPACITY: usize = 1024;
const MAX_READER_FRAME_BYTES: u32 = 256 * 1024 * 1024;

/// Options for starting a trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceStartOptions {
    /// Bundle directory. A generated directory below the zmux state directory
    /// is used when omitted.
    pub output: Option<PathBuf>,
    /// Maximum size of `events.zmuxtrace`, including its file header.
    pub max_bytes: u64,
}

impl Default for TraceStartOptions {
    fn default() -> Self {
        Self {
            output: None,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// A cheap snapshot of the current or most recently completed trace.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStatusSnapshot {
    pub active: bool,
    pub path: Option<PathBuf>,
    pub bytes_written: u64,
    pub dropped_records: u64,
    pub reason: Option<String>,
}

/// Stable event categories in schema version 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    Start,
    Stop,
    Gap,
    ClientAttach,
    ClientDetach,
    ClientInput,
    ClientOutput,
    ClientMessage,
    PaneInput,
    PaneOutput,
    PaneSpawn,
    PaneClose,
    Resize,
    Layout,
    ServerFrame,
    State,
    Warning,
    Error,
    Custom,
}

impl TraceKind {
    fn code(self) -> u8 {
        match self {
            Self::Start => 0,
            Self::Stop => 1,
            Self::Gap => 2,
            Self::ClientAttach => 3,
            Self::ClientDetach => 4,
            Self::ClientInput => 5,
            Self::ClientOutput => 6,
            Self::ClientMessage => 7,
            Self::PaneInput => 8,
            Self::PaneOutput => 9,
            Self::PaneSpawn => 10,
            Self::PaneClose => 11,
            Self::Resize => 12,
            Self::Layout => 13,
            Self::ServerFrame => 14,
            Self::State => 15,
            Self::Warning => 16,
            Self::Error => 17,
            Self::Custom => 18,
        }
    }

    fn from_code(code: u8) -> io::Result<Self> {
        Ok(match code {
            0 => Self::Start,
            1 => Self::Stop,
            2 => Self::Gap,
            3 => Self::ClientAttach,
            4 => Self::ClientDetach,
            5 => Self::ClientInput,
            6 => Self::ClientOutput,
            7 => Self::ClientMessage,
            8 => Self::PaneInput,
            9 => Self::PaneOutput,
            10 => Self::PaneSpawn,
            11 => Self::PaneClose,
            12 => Self::Resize,
            13 => Self::Layout,
            14 => Self::ServerFrame,
            15 => Self::State,
            16 => Self::Warning,
            17 => Self::Error,
            18 => Self::Custom,
            other => return Err(invalid_data(format!("unknown trace kind {other}"))),
        })
    }
}

/// Optional stable identifiers associated with a record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    pub client_id: Option<u64>,
    pub window_id: Option<u32>,
    pub pane_id: Option<u32>,
}

/// The decoded payload of a trace record.
#[derive(Clone, Debug, PartialEq)]
pub enum TracePayload {
    Bytes(Vec<u8>),
    Json(Value),
}

impl TracePayload {
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::Json(_) => None,
        }
    }

    pub fn as_json(&self) -> Option<&Value> {
        match self {
            Self::Json(value) => Some(value),
            Self::Bytes(_) => None,
        }
    }
}

/// One ordered event from an events file.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceRecord {
    pub seq: u64,
    pub elapsed_ns: u64,
    pub kind: TraceKind,
    pub context: TraceContext,
    pub payload: TracePayload,
}

/// Cloneable controller for one session's optional trace.
#[derive(Clone)]
pub struct TraceHub {
    inner: Arc<HubInner>,
}

impl TraceHub {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(HubInner {
                session: session.into(),
                state: Mutex::new(HubState::default()),
                accepting: Arc::new(AtomicBool::new(false)),
            }),
        }
    }

    /// Start tracing and return the private bundle directory.
    pub fn start(&self, options: TraceStartOptions) -> io::Result<PathBuf> {
        self.start_inner(options, DEFAULT_QUEUE_CAPACITY, Duration::ZERO)
    }

    fn start_inner(
        &self,
        options: TraceStartOptions,
        queue_capacity: usize,
        writer_delay: Duration,
    ) -> io::Result<PathBuf> {
        if options.max_bytes < HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("trace size cap must be at least {HEADER_LEN} bytes"),
            ));
        }
        if queue_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "trace queue capacity must be nonzero",
            ));
        }

        let mut state = lock(&self.inner.state);
        refresh_finished(&mut state);
        if state.active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a trace is already active",
            ));
        }

        let started = unix_time();
        let bundle = match options.output {
            Some(path) => path,
            None => default_bundle_path(&self.inner.session, started),
        };
        ensure_private_dir(&bundle)?;
        let events_path = bundle.join("events.zmuxtrace");
        let manifest_path = bundle.join("manifest.json");

        let mut events = private_create_new(&events_path)?;
        events.write_all(MAGIC)?;
        events.write_all(&SCHEMA_VERSION.to_le_bytes())?;
        events.flush()?;

        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            zmux_version: env!("CARGO_PKG_VERSION").to_string(),
            session: self.inner.session.clone(),
            started_unix_ms: started.as_millis_u64(),
            ended_unix_ms: None,
            max_bytes: options.max_bytes,
            events: "events.zmuxtrace".to_string(),
            bytes_written: HEADER_LEN,
            dropped_records: 0,
            reason: None,
        };
        write_new_manifest(&manifest_path, &manifest)?;

        let shared = Arc::new(WriterShared::new(HEADER_LEN));
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let writer_shared = Arc::clone(&shared);
        let writer_accepting = Arc::clone(&self.inner.accepting);
        let writer_manifest = manifest.clone();
        let thread_name = format!("zmux-trace-{}", safe_component(&self.inner.session));
        self.inner.accepting.store(true, Ordering::Release);
        let join = match thread::Builder::new().name(thread_name).spawn(move || {
            run_writer(
                events,
                receiver,
                writer_shared,
                writer_accepting,
                WriterConfig {
                    max_bytes: options.max_bytes,
                    manifest_path,
                    manifest: writer_manifest,
                    delay: writer_delay,
                },
            )
        }) {
            Ok(join) => join,
            Err(error) => {
                self.inner.accepting.store(false, Ordering::Release);
                return Err(error);
            }
        };

        let start_record = QueuedRecord {
            seq: 1,
            elapsed_ns: 0,
            kind: TraceKind::Start,
            context: TraceContext::default(),
            payload: QueuedPayload::Json(
                serde_json::to_vec(&json!({
                    "schema_version": SCHEMA_VERSION,
                    "zmux_version": env!("CARGO_PKG_VERSION"),
                    "session": self.inner.session,
                    "max_bytes": options.max_bytes,
                }))
                .expect("static trace start JSON is serializable"),
            ),
        };
        // A fresh channel always has room for this first record.
        if sender.try_send(start_record).is_err() {
            self.inner.accepting.store(false, Ordering::Release);
            return Err(io::Error::other("trace writer stopped during startup"));
        }

        state.active = Some(ActiveTrace {
            sender,
            join: Some(join),
            shared,
            started: Instant::now(),
            next_seq: 2,
            pending_gap: None,
            path: bundle.clone(),
        });
        state.last = TraceStatusSnapshot {
            active: true,
            path: Some(bundle.clone()),
            bytes_written: HEADER_LEN,
            dropped_records: 0,
            reason: None,
        };
        Ok(bundle)
    }

    pub fn status(&self) -> TraceStatusSnapshot {
        // Status is queried from the daemon event loop. Never reap/join the
        // writer here: after a cap or disk failure it may still be flushing,
        // and that latency must remain isolated to the writer thread. Start,
        // stop, note_failure, and Drop perform the eventual reap.
        let state = lock(&self.inner.state);
        match state.active.as_ref() {
            Some(active) => active.snapshot(),
            None => state.last.clone(),
        }
    }

    /// Return whether records are currently accepted without taking the hub
    /// mutex. Callers can use this to avoid constructing diagnostic payloads
    /// while tracing is disabled.
    pub fn is_active(&self) -> bool {
        self.inner.accepting.load(Ordering::Acquire)
    }

    /// Publish a best-effort start/setup failure without disturbing a live
    /// trace. This lets daemon control replies surface filesystem or writer
    /// setup errors while keeping those failures out of the session path.
    pub fn note_failure(&self, reason: impl Into<String>) {
        let mut state = lock(&self.inner.state);
        refresh_finished(&mut state);
        if state.active.is_none() {
            state.last = TraceStatusSnapshot {
                reason: Some(reason.into()),
                ..TraceStatusSnapshot::default()
            };
        }
    }

    /// Offer raw bytes to the trace writer without waiting for disk I/O.
    pub fn record_bytes(&self, kind: TraceKind, context: TraceContext, bytes: &[u8]) -> bool {
        if !self.inner.accepting.load(Ordering::Acquire) {
            return false;
        }
        self.record_payload(kind, context, QueuedPayload::Bytes(bytes.to_vec()))
    }

    /// Serialize and offer structured data to the trace writer.
    pub fn record_json<T: Serialize + ?Sized>(
        &self,
        kind: TraceKind,
        context: TraceContext,
        value: &T,
    ) -> bool {
        if !self.inner.accepting.load(Ordering::Acquire) {
            return false;
        }
        let Ok(encoded) = serde_json::to_vec(value) else {
            return false;
        };
        self.record_payload(kind, context, QueuedPayload::Json(encoded))
    }

    fn record_payload(
        &self,
        kind: TraceKind,
        context: TraceContext,
        payload: QueuedPayload,
    ) -> bool {
        let mut state = lock(&self.inner.state);
        // Do not join a failed writer from this hot path. `status`, `stop`, or
        // the next `start` reaps it; recording simply becomes a no-op as soon
        // as the writer flips its shared active bit.
        let Some(active) = state.active.as_mut() else {
            return false;
        };
        if !active.shared.active.load(Ordering::Acquire) {
            return false;
        }

        if let Some(gap) = active.pending_gap.as_ref() {
            let gap_record = gap.to_record();
            match active.sender.try_send(gap_record) {
                Ok(()) => active.pending_gap = None,
                Err(TrySendError::Full(_)) => {
                    active.note_drop(payload.encoded_len(), active.elapsed_ns());
                    return false;
                }
                Err(TrySendError::Disconnected(_)) => {
                    active.note_disconnected_drop(payload.encoded_len());
                    self.inner.accepting.store(false, Ordering::Release);
                    return false;
                }
            }
        }

        let record = QueuedRecord {
            seq: active.take_seq(),
            elapsed_ns: active.elapsed_ns(),
            kind,
            context,
            payload,
        };
        match active.sender.try_send(record) {
            Ok(()) => true,
            Err(TrySendError::Full(record)) => {
                active.begin_gap(record.seq, record.elapsed_ns, record.payload.encoded_len());
                false
            }
            Err(TrySendError::Disconnected(record)) => {
                active.note_disconnected_drop(record.payload.encoded_len());
                self.inner.accepting.store(false, Ordering::Release);
                false
            }
        }
    }

    /// Stop tracing, drain the bounded queue, and return its final status.
    pub fn stop(&self) -> TraceStatusSnapshot {
        let active = {
            let mut state = lock(&self.inner.state);
            refresh_finished(&mut state);
            self.inner.accepting.store(false, Ordering::Release);
            state.active.take()
        };

        let Some(active) = active else {
            return self.status();
        };
        let snapshot = finish_active(active);
        let mut state = lock(&self.inner.state);
        state.last = snapshot.clone();
        snapshot
    }
}

impl fmt::Debug for TraceHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TraceHub")
            .field("session", &self.inner.session)
            .field("status", &self.status())
            .finish()
    }
}

struct HubInner {
    session: String,
    state: Mutex<HubState>,
    accepting: Arc<AtomicBool>,
}

impl Drop for HubInner {
    fn drop(&mut self) {
        self.accepting.store(false, Ordering::Release);
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(active) = state.active.take() {
            state.last = finish_active(active);
        }
    }
}

#[derive(Default)]
struct HubState {
    active: Option<ActiveTrace>,
    last: TraceStatusSnapshot,
}

struct ActiveTrace {
    sender: SyncSender<QueuedRecord>,
    join: Option<JoinHandle<()>>,
    shared: Arc<WriterShared>,
    started: Instant,
    next_seq: u64,
    pending_gap: Option<PendingGap>,
    path: PathBuf,
}

impl ActiveTrace {
    fn snapshot(&self) -> TraceStatusSnapshot {
        TraceStatusSnapshot {
            active: self.shared.active.load(Ordering::Acquire),
            path: Some(self.path.clone()),
            bytes_written: self.shared.bytes_written.load(Ordering::Relaxed),
            dropped_records: self.shared.dropped_records.load(Ordering::Relaxed),
            reason: lock(&self.shared.reason).clone(),
        }
    }

    fn elapsed_ns(&self) -> u64 {
        self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }

    fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    fn begin_gap(&mut self, seq: u64, elapsed_ns: u64, bytes: usize) {
        self.shared.dropped_records.fetch_add(1, Ordering::Relaxed);
        self.pending_gap = Some(PendingGap {
            first_seq: seq,
            last_seq: seq,
            first_elapsed_ns: elapsed_ns,
            records: 1,
            bytes: bytes as u64,
        });
    }

    fn note_drop(&mut self, bytes: usize, elapsed_ns: u64) {
        let seq = self.take_seq();
        self.shared.dropped_records.fetch_add(1, Ordering::Relaxed);
        if let Some(gap) = self.pending_gap.as_mut() {
            gap.last_seq = seq;
            gap.records = gap.records.saturating_add(1);
            gap.bytes = gap.bytes.saturating_add(bytes as u64);
        } else {
            self.pending_gap = Some(PendingGap {
                first_seq: seq,
                last_seq: seq,
                first_elapsed_ns: elapsed_ns,
                records: 1,
                bytes: bytes as u64,
            });
        }
    }

    fn note_disconnected_drop(&mut self, bytes: usize) {
        self.note_drop(bytes, self.elapsed_ns());
        self.shared.active.store(false, Ordering::Release);
        set_reason_once(&self.shared, "trace writer is unavailable".to_string());
    }
}

struct PendingGap {
    first_seq: u64,
    last_seq: u64,
    first_elapsed_ns: u64,
    records: u64,
    bytes: u64,
}

impl PendingGap {
    fn to_record(&self) -> QueuedRecord {
        QueuedRecord {
            seq: self.first_seq,
            elapsed_ns: self.first_elapsed_ns,
            kind: TraceKind::Gap,
            context: TraceContext::default(),
            payload: QueuedPayload::Json(
                serde_json::to_vec(&json!({
                    "dropped_records": self.records,
                    "dropped_payload_bytes": self.bytes,
                    "first_seq": self.first_seq,
                    "last_seq": self.last_seq,
                    "reason": "writer queue full",
                }))
                .expect("static gap JSON is serializable"),
            ),
        }
    }
}

struct WriterShared {
    active: AtomicBool,
    bytes_written: AtomicU64,
    dropped_records: AtomicU64,
    reason: Mutex<Option<String>>,
}

impl WriterShared {
    fn new(bytes_written: u64) -> Self {
        Self {
            active: AtomicBool::new(true),
            bytes_written: AtomicU64::new(bytes_written),
            dropped_records: AtomicU64::new(0),
            reason: Mutex::new(None),
        }
    }
}

enum QueuedPayload {
    Bytes(Vec<u8>),
    Json(Vec<u8>),
}

impl QueuedPayload {
    fn encoded_len(&self) -> usize {
        match self {
            Self::Bytes(bytes) | Self::Json(bytes) => bytes.len(),
        }
    }
}

struct QueuedRecord {
    seq: u64,
    elapsed_ns: u64,
    kind: TraceKind,
    context: TraceContext,
    payload: QueuedPayload,
}

struct WriterConfig {
    max_bytes: u64,
    manifest_path: PathBuf,
    manifest: Manifest,
    delay: Duration,
}

#[derive(Clone, Serialize)]
struct Manifest {
    schema_version: u16,
    zmux_version: String,
    session: String,
    started_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_unix_ms: Option<u64>,
    max_bytes: u64,
    events: String,
    bytes_written: u64,
    dropped_records: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

fn run_writer(
    mut file: File,
    receiver: Receiver<QueuedRecord>,
    shared: Arc<WriterShared>,
    accepting: Arc<AtomicBool>,
    mut config: WriterConfig,
) {
    while let Ok(record) = receiver.recv() {
        if !config.delay.is_zero() {
            thread::sleep(config.delay);
        }
        let frame = match encode_record(&record) {
            Ok(frame) => frame,
            Err(error) => {
                set_reason_once(&shared, format!("trace encoding failed: {error}"));
                shared.active.store(false, Ordering::Release);
                accepting.store(false, Ordering::Release);
                count_unwritten(&receiver, &shared, 1);
                break;
            }
        };
        let frame_bytes = 4_u64.saturating_add(frame.len() as u64);
        let written = shared.bytes_written.load(Ordering::Relaxed);
        if written.saturating_add(frame_bytes) > config.max_bytes {
            set_reason_once(
                &shared,
                format!("size cap reached ({} bytes)", config.max_bytes),
            );
            shared.active.store(false, Ordering::Release);
            accepting.store(false, Ordering::Release);
            count_unwritten(&receiver, &shared, 1);
            break;
        }
        if let Err(error) = file
            .write_all(&(frame.len() as u32).to_le_bytes())
            .and_then(|()| file.write_all(&frame))
        {
            set_reason_once(&shared, format!("trace write failed: {error}"));
            shared.active.store(false, Ordering::Release);
            accepting.store(false, Ordering::Release);
            count_unwritten(&receiver, &shared, 1);
            break;
        }
        shared
            .bytes_written
            .fetch_add(frame_bytes, Ordering::Relaxed);
    }

    if let Err(error) = file.flush().and_then(|()| file.sync_all()) {
        set_reason_once(&shared, format!("trace flush failed: {error}"));
    }
    set_reason_once(&shared, "stopped".to_string());
    shared.active.store(false, Ordering::Release);
    accepting.store(false, Ordering::Release);

    config.manifest.ended_unix_ms = Some(unix_time().as_millis_u64());
    config.manifest.bytes_written = shared.bytes_written.load(Ordering::Relaxed);
    config.manifest.dropped_records = shared.dropped_records.load(Ordering::Relaxed);
    config.manifest.reason = lock(&shared.reason).clone();
    if let Err(error) = rewrite_manifest(&config.manifest_path, &config.manifest) {
        *lock(&shared.reason) = Some(format!("manifest update failed: {error}"));
    }
}

fn count_unwritten(receiver: &Receiver<QueuedRecord>, shared: &WriterShared, current: u64) {
    let queued = receiver.try_iter().count() as u64;
    shared
        .dropped_records
        .fetch_add(current.saturating_add(queued), Ordering::Relaxed);
}

fn finish_active(mut active: ActiveTrace) -> TraceStatusSnapshot {
    if active.shared.active.load(Ordering::Acquire) {
        if let Some(gap) = active.pending_gap.take() {
            let _ = active.sender.send(gap.to_record());
        }
        let stop = QueuedRecord {
            seq: active.take_seq(),
            elapsed_ns: active.elapsed_ns(),
            kind: TraceKind::Stop,
            context: TraceContext::default(),
            payload: QueuedPayload::Json(b"{\"reason\":\"requested\"}".to_vec()),
        };
        let _ = active.sender.send(stop);
    }
    let shared = Arc::clone(&active.shared);
    let path = active.path.clone();
    drop(active.sender);
    if let Some(join) = active.join.take() {
        if join.join().is_err() {
            set_reason_once(&shared, "trace writer thread panicked".to_string());
        }
    }
    shared.active.store(false, Ordering::Release);
    TraceStatusSnapshot {
        active: shared.active.load(Ordering::Acquire),
        path: Some(path),
        bytes_written: shared.bytes_written.load(Ordering::Relaxed),
        dropped_records: shared.dropped_records.load(Ordering::Relaxed),
        reason: lock(&shared.reason).clone(),
    }
}

fn refresh_finished(state: &mut HubState) {
    let finished = state
        .active
        .as_ref()
        .is_some_and(|active| !active.shared.active.load(Ordering::Acquire));
    if finished && let Some(active) = state.active.take() {
        state.last = finish_active(active);
    }
}

fn encode_record(record: &QueuedRecord) -> io::Result<Vec<u8>> {
    let payload = match &record.payload {
        QueuedPayload::Bytes(bytes) | QueuedPayload::Json(bytes) => bytes,
    };
    let mut len = 19_usize.saturating_add(payload.len());
    if record.context.client_id.is_some() {
        len = len.saturating_add(8);
    }
    if record.context.window_id.is_some() {
        len = len.saturating_add(4);
    }
    if record.context.pane_id.is_some() {
        len = len.saturating_add(4);
    }
    if len > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trace record exceeds the version 1 frame limit",
        ));
    }

    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&record.seq.to_le_bytes());
    out.extend_from_slice(&record.elapsed_ns.to_le_bytes());
    out.push(record.kind.code());
    let mut flags = 0_u8;
    if record.context.client_id.is_some() {
        flags |= 1;
    }
    if record.context.window_id.is_some() {
        flags |= 2;
    }
    if record.context.pane_id.is_some() {
        flags |= 4;
    }
    out.push(flags);
    if let Some(id) = record.context.client_id {
        out.extend_from_slice(&id.to_le_bytes());
    }
    if let Some(id) = record.context.window_id {
        out.extend_from_slice(&id.to_le_bytes());
    }
    if let Some(id) = record.context.pane_id {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out.push(match record.payload {
        QueuedPayload::Bytes(_) => 0,
        QueuedPayload::Json(_) => 1,
    });
    out.extend_from_slice(payload);
    Ok(out)
}

fn decode_record(frame: &[u8]) -> io::Result<TraceRecord> {
    let mut cursor = FrameCursor::new(frame);
    let seq = cursor.u64()?;
    let elapsed_ns = cursor.u64()?;
    let kind = TraceKind::from_code(cursor.u8()?)?;
    let flags = cursor.u8()?;
    if flags & !0b111 != 0 {
        return Err(invalid_data("trace context has unknown flags"));
    }
    let context = TraceContext {
        client_id: if flags & 1 != 0 {
            Some(cursor.u64()?)
        } else {
            None
        },
        window_id: if flags & 2 != 0 {
            Some(cursor.u32()?)
        } else {
            None
        },
        pane_id: if flags & 4 != 0 {
            Some(cursor.u32()?)
        } else {
            None
        },
    };
    let payload_kind = cursor.u8()?;
    let bytes = cursor.remaining();
    let payload = match payload_kind {
        0 => TracePayload::Bytes(bytes.to_vec()),
        1 => TracePayload::Json(
            serde_json::from_slice(bytes)
                .map_err(|error| invalid_data(format!("invalid JSON trace payload: {error}")))?,
        ),
        other => return Err(invalid_data(format!("unknown trace payload kind {other}"))),
    };
    Ok(TraceRecord {
        seq,
        elapsed_ns,
        kind,
        context,
        payload,
    })
}

struct FrameCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> FrameCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or_else(|| invalid_data("trace frame offset overflow"))?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| invalid_data("truncated trace record"))?;
        self.offset = end;
        Ok(slice.try_into().expect("slice length was checked"))
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take::<1>()?[0])
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }
}

/// Streaming reader for a bundle directory or an `events.zmuxtrace` file.
pub struct TraceReader {
    file: File,
    finished: bool,
}

impl TraceReader {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let events_path = if path.is_dir() {
            path.join("events.zmuxtrace")
        } else {
            path.to_path_buf()
        };
        let mut file = File::open(events_path)?;
        let mut magic = [0_u8; MAGIC.len()];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(invalid_data("not a zmux trace events file"));
        }
        let mut version = [0_u8; 2];
        file.read_exact(&mut version)?;
        let version = u16::from_le_bytes(version);
        if version != SCHEMA_VERSION {
            return Err(invalid_data(format!(
                "unsupported trace schema version {version}"
            )));
        }
        Ok(Self {
            file,
            finished: false,
        })
    }

    pub fn read_all(mut self) -> io::Result<Vec<TraceRecord>> {
        self.by_ref().collect()
    }
}

impl Iterator for TraceReader {
    type Item = io::Result<TraceRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let mut length = [0_u8; 4];
        match self.file.read(&mut length[..1]) {
            Ok(0) => {
                self.finished = true;
                return None;
            }
            Ok(1) => {}
            Ok(_) => unreachable!("one-byte read returned more than one byte"),
            Err(error) => {
                self.finished = true;
                return Some(Err(error));
            }
        }
        if let Err(error) = self.file.read_exact(&mut length[1..]) {
            self.finished = true;
            return Some(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("truncated trace frame length: {error}"),
            )));
        }
        let length = u32::from_le_bytes(length);
        if length > MAX_READER_FRAME_BYTES {
            self.finished = true;
            return Some(Err(invalid_data(format!(
                "trace frame length {length} exceeds reader limit {MAX_READER_FRAME_BYTES}"
            ))));
        }
        let mut frame = vec![0_u8; length as usize];
        if let Err(error) = self.file.read_exact(&mut frame) {
            self.finished = true;
            return Some(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("truncated trace frame body: {error}"),
            )));
        }
        Some(decode_record(&frame))
    }
}

fn default_bundle_path(session: &str, now: UnixTime) -> PathBuf {
    let parent = state_dir().join("traces").join(safe_component(session));
    // The bundle itself is hardened by `ensure_private_dir`; harden its state
    // ancestors as well so directory listings do not disclose session names.
    let _ = ensure_private_dir(&parent);
    parent.join(format!(
        "{}-{:09}-{}",
        now.seconds,
        now.nanos,
        std::process::id()
    ))
}

#[derive(Clone, Copy)]
struct UnixTime {
    seconds: u64,
    nanos: u32,
}

impl UnixTime {
    fn as_millis_u64(self) -> u64 {
        self.seconds
            .saturating_mul(1000)
            .saturating_add(u64::from(self.nanos / 1_000_000))
    }
}

fn unix_time() -> UnixTime {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    UnixTime {
        seconds: elapsed.as_secs(),
        nanos: elapsed.subsec_nanos(),
    }
}

fn private_create_new(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn write_new_manifest(path: &Path, manifest: &Manifest) -> io::Result<()> {
    let mut file = private_create_new(path)?;
    serde_json::to_writer_pretty(&mut file, manifest).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.flush()
}

fn rewrite_manifest(path: &Path, manifest: &Manifest) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer_pretty(&mut file, manifest).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.flush()
}

fn set_reason_once(shared: &WriterShared, reason: String) {
    let mut slot = lock(&shared.reason);
    if slot.is_none() {
        *slot = Some(reason);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_bundle(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zmux-trace-test-{}-{}-{}",
            std::process::id(),
            name,
            unix_time().nanos
        ))
    }

    #[test]
    fn disabled_hub_is_a_noop() {
        let hub = TraceHub::new("off");
        assert!(!hub.is_active());
        assert!(!hub.record_bytes(TraceKind::PaneOutput, TraceContext::default(), b"secret"));
        assert!(!hub.record_json(TraceKind::State, TraceContext::default(), &json!({"x": 1})));
        assert_eq!(hub.status(), TraceStatusSnapshot::default());
        assert_eq!(hub.stop(), TraceStatusSnapshot::default());
    }

    #[test]
    fn setup_failure_is_visible_but_does_not_replace_an_active_trace() {
        let hub = TraceHub::new("failure-status");
        hub.note_failure("cannot create trace bundle");
        assert_eq!(
            hub.status().reason.as_deref(),
            Some("cannot create trace bundle")
        );

        let bundle = temp_bundle("failure-status-active");
        hub.start(TraceStartOptions {
            output: Some(bundle.clone()),
            max_bytes: 1024 * 1024,
        })
        .unwrap();
        assert!(hub.is_active());
        hub.note_failure("must not replace active status");
        let status = hub.status();
        assert!(status.active);
        assert_eq!(status.reason, None);
        let completed = hub.stop();
        assert!(completed.path.is_some());
        assert!(completed.bytes_written > 0);

        hub.note_failure("second start failed");
        assert_eq!(
            hub.status(),
            TraceStatusSnapshot {
                reason: Some("second start failed".to_string()),
                ..TraceStatusSnapshot::default()
            }
        );
        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn status_does_not_join_a_writer_that_is_still_finishing() {
        let bundle = temp_bundle("nonblocking-status");
        let hub = TraceHub::new("nonblocking-status");
        hub.start_inner(
            TraceStartOptions {
                output: Some(bundle.clone()),
                max_bytes: 1024 * 1024,
            },
            1,
            Duration::from_millis(250),
        )
        .unwrap();

        // Model the state immediately after a writer detects a cap or I/O
        // failure but before its delayed flush/final manifest update ends.
        {
            let state = lock(&hub.inner.state);
            let active = state.active.as_ref().unwrap();
            active.shared.active.store(false, Ordering::Release);
            hub.inner.accepting.store(false, Ordering::Release);
        }

        let started = Instant::now();
        let snapshot = hub.status();
        assert!(!snapshot.active);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "status waited for the delayed writer"
        );

        // Explicit stop owns the potentially blocking flush/reap boundary.
        hub.stop();
        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn bytes_and_json_round_trip_in_order() {
        let bundle = temp_bundle("roundtrip");
        let hub = TraceHub::new("work/a");
        hub.start(TraceStartOptions {
            output: Some(bundle.clone()),
            max_bytes: 1024 * 1024,
        })
        .unwrap();
        let context = TraceContext {
            client_id: Some(9),
            window_id: Some(3),
            pane_id: Some(42),
        };
        assert!(hub.record_bytes(TraceKind::PaneOutput, context, b"\x1b[31mhello"));
        assert!(hub.record_json(
            TraceKind::Resize,
            TraceContext {
                pane_id: Some(42),
                ..TraceContext::default()
            },
            &json!({"rows": 40, "cols": 120})
        ));
        let status = hub.stop();
        assert!(!hub.is_active());
        assert!(!status.active);
        assert_eq!(status.reason.as_deref(), Some("stopped"));

        let records = TraceReader::open(&bundle).unwrap().read_all().unwrap();
        assert_eq!(
            records.iter().map(|record| record.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(records[0].kind, TraceKind::Start);
        assert_eq!(
            records[0].payload.as_json().unwrap()["zmux_version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(records[1].kind, TraceKind::PaneOutput);
        assert_eq!(records[1].context, context);
        assert_eq!(
            records[1].payload,
            TracePayload::Bytes(b"\x1b[31mhello".to_vec())
        );
        assert_eq!(records[2].kind, TraceKind::Resize);
        assert_eq!(records[2].payload.as_json().unwrap()["cols"], 120);
        assert_eq!(records[3].kind, TraceKind::Stop);
        assert!(
            records
                .windows(2)
                .all(|pair| pair[0].elapsed_ns <= pair[1].elapsed_ns)
        );

        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn bundle_and_files_are_private() {
        let bundle = temp_bundle("permissions");
        let hub = TraceHub::new("private");
        hub.start(TraceStartOptions {
            output: Some(bundle.clone()),
            max_bytes: 1024 * 1024,
        })
        .unwrap();
        hub.stop();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&bundle), 0o700);
        assert_eq!(mode(&bundle.join("manifest.json")), 0o600);
        assert_eq!(mode(&bundle.join("events.zmuxtrace")), 0o600);
        let manifest: Value =
            serde_json::from_slice(&fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["zmux_version"], env!("CARGO_PKG_VERSION"));

        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn complete_records_survive_a_crash_truncated_tail() {
        let bundle = temp_bundle("truncated");
        let hub = TraceHub::new("truncated");
        hub.start(TraceStartOptions {
            output: Some(bundle.clone()),
            max_bytes: 1024 * 1024,
        })
        .unwrap();
        assert!(hub.record_bytes(TraceKind::PaneOutput, TraceContext::default(), b"complete"));
        hub.stop();

        let events = bundle.join("events.zmuxtrace");
        let file = OpenOptions::new().write(true).open(&events).unwrap();
        file.set_len(file.metadata().unwrap().len() - 2).unwrap();

        let mut reader = TraceReader::open(&events).unwrap();
        assert_eq!(reader.next().unwrap().unwrap().kind, TraceKind::Start);
        assert_eq!(reader.next().unwrap().unwrap().kind, TraceKind::PaneOutput);
        assert_eq!(
            reader.next().unwrap().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(reader.next().is_none());

        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn cap_disables_capture_without_exceeding_limit() {
        let bundle = temp_bundle("cap");
        let hub = TraceHub::new("cap");
        let cap = 256;
        hub.start(TraceStartOptions {
            output: Some(bundle.clone()),
            max_bytes: cap,
        })
        .unwrap();
        for _ in 0..20 {
            hub.record_bytes(TraceKind::PaneOutput, TraceContext::default(), &[b'x'; 96]);
        }
        let status = hub.stop();
        assert!(status.bytes_written <= cap);
        assert!(status.dropped_records > 0);
        assert!(
            status
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("size cap"))
        );
        assert!(fs::metadata(bundle.join("events.zmuxtrace")).unwrap().len() <= cap);
        TraceReader::open(&bundle).unwrap().read_all().unwrap();

        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn queue_pressure_is_reported_and_emits_a_gap_after_recovery() {
        let bundle = temp_bundle("drops");
        let hub = TraceHub::new("drops");
        hub.start_inner(
            TraceStartOptions {
                output: Some(bundle.clone()),
                max_bytes: 1024 * 1024,
            },
            1,
            Duration::from_millis(15),
        )
        .unwrap();

        for _ in 0..30 {
            hub.record_bytes(TraceKind::PaneOutput, TraceContext::default(), b"busy");
        }
        thread::sleep(Duration::from_millis(80));
        hub.record_bytes(TraceKind::State, TraceContext::default(), b"recovered");
        let status = hub.stop();
        assert!(status.dropped_records > 0);

        let records = TraceReader::open(&bundle).unwrap().read_all().unwrap();
        assert!(records.iter().any(|record| record.kind == TraceKind::Gap));
        assert!(records.windows(2).all(|pair| pair[0].seq < pair[1].seq));

        fs::remove_dir_all(bundle).unwrap();
    }
}
