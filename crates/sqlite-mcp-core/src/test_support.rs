//! Rust-only worker instrumentation. The `test-support` feature is never
//! enabled by the normal locked production build; hooks are inert unless the
//! feature and `SQLITE_MCP_TEST_SUPPORT=1` marker are both present.
//!
//! Harness guarantees (Phase 0):
//! - Events are keyed, retained, and lossless within a bounded per-event queue;
//!   arm→trigger→acknowledge→release cannot drop an emission.
//! - Every wait and every worker-side barrier block is bounded.
//! - Clock injection applies only when the feature and marker are both active;
//!   ordinary production builds always use monotonic real time.
//! - Cleanup-fault injection is a Rust-only test API, never MCP/config input.
use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Condvar, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Event {
    BeginCompletion,
    Prepare,
    StepProgress,
    BusyRetry,
    CommitEntry,
    CommitReturn,
    RollbackCompletion,
    RequestHandover,
    // Ordered-operation lifecycle events (admission protocol).
    AdmissionEnqueue,
    AdmissionDequeue,
    ClosureEntry,
    PostWorkerPrePublication,
    ControlEntry,
    ControlReturn,
    // Shutdown lifecycle events.
    ShutdownRequested,
    ShutdownCleanup,
    ShutdownClosed,
    // Schema observation publication.
    SchemaVersionRead,
    // Create-identity checkpoints (F-05).
    CreationDescriptorCaptured,
    CreationPreOpenCheckpoint,
    CreationPostOpenCheckpoint,
    CreationPostInitCheckpoint,
    // Harness-only: emitted by instrumented children on parent command.
    HarnessCommand,
    // Bounded busy-window guard lifecycle (worker command arms).
    GuardInstalled,
    FirstSqliteOperation,
    GuardCleared,
}

pub const ALL_EVENTS: &[Event] = &[
    Event::BeginCompletion,
    Event::Prepare,
    Event::StepProgress,
    Event::BusyRetry,
    Event::CommitEntry,
    Event::CommitReturn,
    Event::RollbackCompletion,
    Event::RequestHandover,
    Event::AdmissionEnqueue,
    Event::AdmissionDequeue,
    Event::ClosureEntry,
    Event::PostWorkerPrePublication,
    Event::ControlEntry,
    Event::ControlReturn,
    Event::ShutdownRequested,
    Event::ShutdownCleanup,
    Event::ShutdownClosed,
    Event::SchemaVersionRead,
    Event::CreationDescriptorCaptured,
    Event::CreationPreOpenCheckpoint,
    Event::CreationPostOpenCheckpoint,
    Event::CreationPostInitCheckpoint,
    Event::HarnessCommand,
    Event::GuardInstalled,
    Event::FirstSqliteOperation,
    Event::GuardCleared,
];

/// Identifies which worker/request or creation operation emitted an event.
/// `generation` increments per lifecycle incarnation so re-armed waits cannot
/// match a stale emission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventKey {
    pub operation_id: u64,
    pub generation: u64,
    pub worker_id: Option<String>,
    pub creation_id: Option<String>,
}

impl EventKey {
    pub fn operation(operation_id: u64) -> Self {
        EventKey {
            operation_id,
            generation: 0,
            worker_id: None,
            creation_id: None,
        }
    }

    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }
}

/// One retained emission. `seq` is monotonic within an event kind; `order`
/// is the process-global emission ordinal and is the only field valid for
/// comparing emission order across different event kinds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRecord {
    pub seq: u64,
    pub event: Event,
    pub key: Option<EventKey>,
    pub order: u64,
}

/// Process-global emission ordinal shared by all event kinds.
static GLOBAL_ORDER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct ArmedGate {
    key: Option<EventKey>,
    released: bool,
    /// Emissions with `seq < cursor` predate this arm and cannot satisfy it.
    cursor: u64,
    /// Worker-side bound: an emit blocked on this gate continues after this
    /// deadline so production work can never hang for the arm's lifetime.
    deadline: Instant,
}

#[derive(Debug, Default, Clone)]
struct Slot {
    records: VecDeque<EventRecord>,
    dropped: u64,
    next_seq: u64,
    armed: Option<ArmedGate>,
}

/// Maximum retained records per event; oldest records are dropped (counted)
/// beyond this bound so long tests cannot grow memory without limit.
const RETAINED_PER_EVENT: usize = 4096;
/// Maximum time an `emit` blocks on an armed gate before continuing.
const EMIT_BLOCK_LIMIT: Duration = Duration::from_secs(10);

static REGISTRY: OnceLock<(Mutex<HashMap<Event, Slot>>, Condvar)> = OnceLock::new();
fn registry() -> &'static (Mutex<HashMap<Event, Slot>>, Condvar) {
    REGISTRY.get_or_init(|| (Mutex::new(HashMap::new()), Condvar::new()))
}

pub fn enabled() -> bool {
    std::env::var("SQLITE_MCP_TEST_SUPPORT").ok().as_deref() == Some("1")
}

static CLOCK_MS: AtomicU64 = AtomicU64::new(0);
static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// Test clock in milliseconds. Injected values apply only when the
/// `test-support` feature and the runtime marker are both present; otherwise
/// this is monotonic real time since first use.
pub fn now_ms() -> u64 {
    let injected = CLOCK_MS.load(Ordering::SeqCst);
    if injected != 0 && enabled() {
        return injected;
    }
    ORIGIN.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub fn set_clock_ms(value: u64) {
    CLOCK_MS.store(value, Ordering::SeqCst);
}

/// Arm an event barrier. The next emission after this arm is observed and
/// pauses the emitter until `release` (bounded by [`EMIT_BLOCK_LIMIT`]).
pub fn arm(event: Event) -> Barrier {
    arm_keyed(event, None)
}

/// Arm a keyed event barrier: only an emission whose key matches `key`
/// (operation id and generation) triggers the pause.
pub fn arm_keyed(event: Event, key: Option<EventKey>) -> Barrier {
    let (lock, _) = registry();
    let mut map = lock.lock().expect("test-support registry poisoned");
    let cursor = map.entry(event).or_default().next_seq;
    map.entry(event).or_default().armed = Some(ArmedGate {
        key,
        released: false,
        cursor,
        deadline: Instant::now() + EMIT_BLOCK_LIMIT,
    });
    Barrier {
        event,
        acknowledged: Arc::new(AtomicBool::new(false)),
    }
}

/// Wait until the armed (or next) event is emitted, returning the retained
/// record. Lossless: an emission that arrived after `arm` but before this call
/// is still observed. Bounded by `deadline`.
pub fn wait_for(event: Event, deadline: Duration) -> Result<EventRecord, String> {
    wait_keyed(event, None, deadline)
}

/// Wait for an emission matching `key` (operation id and generation). See
/// [`wait_for`] for losslessness and bounding.
pub fn wait_keyed(
    event: Event,
    key: Option<&EventKey>,
    deadline: Duration,
) -> Result<EventRecord, String> {
    let start = Instant::now();
    let (lock, cv) = registry();
    let mut map = lock
        .lock()
        .map_err(|_| "test-support registry poisoned".to_string())?;
    // Cursor: a keyed wait is lossless over all retained records; an unarmed
    // unkeyed wait (legacy `wait_for`) observes the next emission after
    // entry; an armed gate observes emissions after its arm.
    let cursor = if key.is_some() {
        0
    } else {
        match map.get(&event).and_then(|slot| slot.armed.as_ref()) {
            Some(gate) => gate.cursor,
            None => map.get(&event).map(|slot| slot.next_seq).unwrap_or(0),
        }
    };
    loop {
        let slot = map.entry(event).or_default();
        if let Some(record) = slot
            .records
            .iter()
            .find(|record| record.seq >= cursor && key_matches(key, record))
        {
            return Ok(record.clone());
        }
        let left = deadline.saturating_sub(start.elapsed());
        if left.is_zero() {
            let slot = map.get(&event).cloned().unwrap_or_default();
            return Err(format!(
                "event {event:?} (key {key:?}) not observed before {deadline:?}; \
                 retained={}, dropped={}, armed={}",
                slot.records.len(),
                slot.dropped,
                slot.armed.is_some()
            ));
        }
        let (next, timeout) = cv
            .wait_timeout(map, left)
            .map_err(|_| "test-support registry poisoned".to_string())?;
        map = next;
        if timeout.timed_out() {
            let slot = map.get(&event).cloned().unwrap_or_default();
            let satisfied = slot
                .records
                .iter()
                .any(|record| record.seq >= cursor && key_matches(key, record));
            if !satisfied {
                return Err(format!(
                    "event {event:?} (key {key:?}) not observed before {deadline:?}; \
                     retained={}, dropped={}, armed={}",
                    slot.records.len(),
                    slot.dropped,
                    slot.armed.is_some()
                ));
            }
        }
    }
}

fn key_matches(expected: Option<&EventKey>, record: &EventRecord) -> bool {
    match (expected, &record.key) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(expected), Some(actual)) => {
            expected.operation_id == actual.operation_id && expected.generation == actual.generation
        }
    }
}

fn gate_matches(gate: &ArmedGate, record: &EventRecord) -> bool {
    record.seq >= gate.cursor && key_matches(gate.key.as_ref(), record)
}

/// Emit an event. Inert unless the feature and marker are both active.
/// Retained losslessly; if an armed gate matches, this pauses (bounded) until
/// [`Barrier::release`].
pub fn emit(event: Event) {
    emit_keyed(event, None);
}

/// Emit a keyed event. See [`emit`].
pub fn emit_keyed(event: Event, key: Option<EventKey>) {
    if !enabled() {
        return;
    }
    emit_keyed_always(event, key);
}

/// Registry mechanics without the marker gate; used by in-process unit tests.
/// Gating itself is proven separately (`production_test_hooks_inert`).
fn emit_keyed_always(event: Event, key: Option<EventKey>) {
    let (lock, cv) = registry();
    let mut map = lock.lock().expect("test-support registry poisoned");
    let slot = map.entry(event).or_default();
    let seq = slot.next_seq;
    slot.next_seq = seq.saturating_add(1);
    let order = GLOBAL_ORDER.fetch_add(1, Ordering::SeqCst);
    slot.records.push_back(EventRecord {
        seq,
        event,
        key,
        order,
    });
    while slot.records.len() > RETAINED_PER_EVENT {
        slot.records.pop_front();
        slot.dropped = slot.dropped.saturating_add(1);
    }
    cv.notify_all();
    let gate = match slot.armed.as_mut() {
        Some(gate) if gate_matches(gate, slot.records.back().expect("just pushed")) => gate,
        _ => return,
    };
    let deadline = gate.deadline;
    loop {
        let released = map
            .get(&event)
            .and_then(|slot| slot.armed.as_ref())
            .map(|gate| gate.released)
            .unwrap_or(true);
        if released {
            break;
        }
        let left = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if left.is_zero() {
            // Bounded worker-side release: continue rather than hang.
            if let Some(slot) = map.get_mut(&event)
                && let Some(gate) = slot.armed.as_mut()
            {
                gate.released = true;
            }
            cv.notify_all();
            break;
        }
        let (next, timeout) = cv
            .wait_timeout(map, left)
            .expect("test-support registry poisoned");
        map = next;
        if timeout.timed_out() {
            continue;
        }
    }
}

#[derive(Clone)]
pub struct Barrier {
    event: Event,
    acknowledged: Arc<AtomicBool>,
}

impl Barrier {
    /// Acknowledge the observed emission and release the paused emitter.
    pub fn release(&self) {
        self.acknowledged.store(true, Ordering::SeqCst);
        let (lock, cv) = registry();
        if let Ok(mut map) = lock.lock() {
            if let Some(slot) = map.get_mut(&self.event)
                && let Some(gate) = slot.armed.as_mut()
            {
                gate.released = true;
            }
            cv.notify_all();
        }
    }

    pub fn is_released(&self) -> bool {
        let (lock, _) = registry();
        match lock.lock() {
            Ok(map) => map
                .get(&self.event)
                .and_then(|slot| slot.armed.as_ref())
                .map(|gate| gate.released)
                .unwrap_or(true),
            Err(_) => true,
        }
    }

    /// Bounded wait for release of this barrier's gate.
    pub fn wait(&self, deadline: Duration) -> Result<(), String> {
        let start = Instant::now();
        let (lock, cv) = registry();
        let mut map = lock
            .lock()
            .map_err(|_| "test-support registry poisoned".to_string())?;
        loop {
            let released = map
                .get(&self.event)
                .and_then(|slot| slot.armed.as_ref())
                .map(|gate| gate.released)
                .unwrap_or(true)
                || self.acknowledged.load(Ordering::SeqCst);
            if released {
                return Ok(());
            }
            let left = deadline.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Err(format!("test barrier {:?} was not released", self.event));
            }
            let (next, _) = cv
                .wait_timeout(map, left)
                .map_err(|_| "test-support registry poisoned".to_string())?;
            map = next;
        }
    }
}

/// Whether an armed gate for `event` would match an emission carrying `key`.
/// The child harness uses this to decide whether an `emit` command must run on
/// a background thread (it would pause on the gate) or inline (deterministic
/// completion before the acknowledgement).
pub fn arm_matches_pending(event: Event, key: Option<&EventKey>) -> bool {
    let (lock, _) = registry();
    lock.lock()
        .map(
            |map| match map.get(&event).and_then(|slot| slot.armed.as_ref()) {
                Some(gate) => match (&gate.key, key) {
                    (None, _) => true,
                    (Some(_), None) => true,
                    (Some(gate_key), Some(emission_key)) => {
                        gate_key.operation_id == emission_key.operation_id
                            && gate_key.generation == emission_key.generation
                    }
                },
                None => false,
            },
        )
        .unwrap_or(false)
}

/// Release any armed gate for `event` without a Barrier handle. Used by the
/// instrumented-child control protocol.
pub fn release_arm(event: Event) {
    let (lock, cv) = registry();
    if let Ok(mut map) = lock.lock()
        && let Some(slot) = map.get_mut(&event)
        && let Some(gate) = slot.armed.as_mut()
    {
        gate.released = true;
        cv.notify_all();
    }
}

/// Whether any armed gate for `event` has been released (introspection for
/// acknowledge-before-release assertions).
pub fn gate_released(event: Event) -> bool {
    let (lock, _) = registry();
    lock.lock()
        .map(|map| {
            map.get(&event)
                .and_then(|slot| slot.armed.as_ref())
                .map(|gate| gate.released)
                .unwrap_or(true)
        })
        .unwrap_or(true)
}

/// Number of retained records for `event` (losslessness and diagnostics).
pub fn count(event: Event) -> usize {
    let (lock, _) = registry();
    lock.lock()
        .map(|map| map.get(&event).map(|slot| slot.records.len()).unwrap_or(0))
        .unwrap_or(0)
}

/// Snapshot of the retained records for `event`, in emission order. Worker
/// fixtures use this to filter by key label and assert event ordering via
/// the monotonic `seq`.
pub fn retained_records(event: Event) -> Vec<EventRecord> {
    let (lock, _) = registry();
    lock.lock()
        .map(|map| {
            map.get(&event)
                .map(|slot| slot.records.iter().cloned().collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// Clear all retained records, arms, and fault injections, and reset the
/// injected clock. Called by instrumented children at entry so process-global
/// state never leaks across child incarnations.
pub fn reset_registry() {
    let (lock, cv) = registry();
    if let Ok(mut map) = lock.lock() {
        map.clear();
        cv.notify_all();
    }
    if let Ok(mut faults) = faults().lock() {
        faults.clear();
    }
    if let Ok(mut fault) = commit_fault().lock() {
        *fault = None;
    }
    set_clock_ms(0);
}

/// A pending injected cleanup failure. Taken at most once by production code
/// at the corresponding cleanup site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupFault {
    pub stage: crate::operation::CleanupStage,
    pub detail: String,
}

static FAULTS: OnceLock<Mutex<HashMap<String, CleanupFault>>> = OnceLock::new();
fn faults() -> &'static Mutex<HashMap<String, CleanupFault>> {
    FAULTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Inject a one-shot cleanup failure for `stage` (Rust-only test API).
pub fn inject_cleanup_fault(stage: crate::operation::CleanupStage, detail: impl Into<String>) {
    let fault = CleanupFault {
        stage,
        detail: detail.into(),
    };
    faults()
        .lock()
        .expect("test-support fault registry poisoned")
        .insert(format!("{:?}", stage), fault);
}

/// Take the pending fault for `stage`, if any. Production cleanup sites call
/// this at the failure boundary the fault models.
pub fn take_cleanup_fault(stage: crate::operation::CleanupStage) -> Option<CleanupFault> {
    faults()
        .lock()
        .expect("test-support fault registry poisoned")
        .remove(&format!("{stage:?}"))
}

/// One-shot commit fault modes used only by Rust tests. The worker consumes the
/// mode at the commit boundary and still probes SQLite on the same worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitFault {
    OpenContinuable,
    AutocommitRestoredUnconfirmed,
    AutocommitRestored,
    Uncertain,
}

static COMMIT_FAULT: OnceLock<Mutex<Option<CommitFault>>> = OnceLock::new();
fn commit_fault() -> &'static Mutex<Option<CommitFault>> {
    COMMIT_FAULT.get_or_init(|| Mutex::new(None))
}

pub fn inject_commit_fault(fault: CommitFault) {
    if enabled() {
        *commit_fault()
            .lock()
            .expect("test-support commit fault registry poisoned") = Some(fault);
    }
}

pub(crate) fn take_commit_fault() -> Option<CommitFault> {
    if !enabled() {
        return None;
    }
    commit_fault()
        .lock()
        .expect("test-support commit fault registry poisoned")
        .take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_records_are_retained_and_match() {
        let key = EventKey::operation(7).with_generation(1);
        // arm → emit (blocked) → observe → release.
        let barrier = arm_keyed(Event::HarnessCommand, Some(key.clone()));
        let emitter =
            std::thread::spawn(move || emit_keyed_always(Event::HarnessCommand, Some(key)));
        let record = wait_for(Event::HarnessCommand, Duration::from_secs(2)).unwrap();
        assert_eq!(record.key.as_ref().unwrap().operation_id, 7);
        assert_eq!(record.key.as_ref().unwrap().generation, 1);
        assert!(record.key.as_ref().unwrap().worker_id.is_none());
        barrier.release();
        emitter.join().unwrap();
    }

    #[test]
    fn waits_are_bounded_and_keyed() {
        let start = Instant::now();
        let key = EventKey::operation(99);
        assert!(wait_keyed(Event::HarnessCommand, Some(&key), Duration::from_millis(50)).is_err());
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert!(start.elapsed() < Duration::from_secs(2));
        // A different key must not satisfy this wait.
        let barrier = arm_keyed(Event::HarnessCommand, Some(EventKey::operation(100)));
        let emitter = std::thread::spawn(|| {
            emit_keyed_always(Event::HarnessCommand, Some(EventKey::operation(101)))
        });
        let record = wait_for(Event::HarnessCommand, Duration::from_millis(300)).unwrap();
        assert_eq!(record.key.as_ref().unwrap().operation_id, 101);
        assert_ne!(record.key.as_ref().unwrap().operation_id, 100);
        barrier.release();
        emitter.join().unwrap();
    }
}
