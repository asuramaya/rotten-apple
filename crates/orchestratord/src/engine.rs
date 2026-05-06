//! Memory control-loop engine + event bus.
//!
//! One std thread, owns a clone of the `ActorHandle`, ticks at 1 Hz. Per
//! domain with a `PolicyMemory` set it reads current vs floor/ceiling and
//! plans a single balloon move per tick — never below `min_mb`, never
//! above `max_mb`. Cooldown is per (domid, direction).
//!
//! The engine never panics. Every libxl call is fallible: the actor may
//! return `BackendUnavailable` indefinitely (no Xen) and the engine just
//! logs once per tick and sleeps. The daemon staying alive without libxl
//! is normal in dev.
//!
//! Policies live in the engine's own `RwLock<HashMap>` rather than in the
//! actor — the actor only owns libxl. `engine.set_policy` mutates this map
//! directly; the tick loop reads it.

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use rotten_apple_manifest::PolicyMemory;

use crate::actor::{ActorError, ActorHandle};
use crate::oneshot;

// ---------------------------------------------------------------------------
// Broadcast bus
//
// Many subscribers, bounded ring buffer. No condvars: clients poll via
// `events.tail`, which is a cheap drain on a Mutex<VecDeque>.

pub mod broadcast {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Ring capacity. 1024 is plenty for the 1 Hz engine + occasional
    /// state-change events; oldest is dropped when full.
    pub const CAPACITY: usize = 1024;

    struct Inner<T> {
        ring: VecDeque<(u64, T)>,
        next_seq: u64,
    }

    pub struct Sender<T> {
        inner: Arc<Mutex<Inner<T>>>,
    }

    /// Receiver carries no state of its own — the cursor is supplied per
    /// `drain_since` call. This matches the `events.tail { since }` shape
    /// where the client is the source of truth for "what have I seen".
    pub struct Receiver<T> {
        inner: Arc<Mutex<Inner<T>>>,
    }

    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            Sender { inner: self.inner.clone() }
        }
    }

    impl<T> Clone for Receiver<T> {
        fn clone(&self) -> Self {
            Receiver { inner: self.inner.clone() }
        }
    }

    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let inner = Arc::new(Mutex::new(Inner {
            ring: VecDeque::with_capacity(CAPACITY),
            next_seq: 1,
        }));
        (Sender { inner: inner.clone() }, Receiver { inner })
    }

    impl<T: Clone> Sender<T> {
        /// Push a value. Oldest is evicted when the ring is full. Returns
        /// the sequence assigned.
        pub fn send(&self, value: T) -> u64 {
            let mut g = self.inner.lock().expect("broadcast inner poisoned");
            let seq = g.next_seq;
            g.next_seq += 1;
            if g.ring.len() == CAPACITY {
                g.ring.pop_front();
            }
            g.ring.push_back((seq, value));
            seq
        }

        pub fn subscribe(&self) -> Receiver<T> {
            Receiver { inner: self.inner.clone() }
        }
    }

    impl<T: Clone> Receiver<T> {
        /// Return all events with seq strictly greater than `cursor`, plus
        /// the latest seq the receiver should remember. If the ring has
        /// rotated past `cursor` only the surviving tail is returned —
        /// callers can detect the gap by comparing the returned cursor
        /// against `cursor + result.len()`.
        pub fn drain_since(&self, cursor: u64) -> (u64, Vec<T>) {
            let g = self.inner.lock().expect("broadcast inner poisoned");
            let mut out = Vec::new();
            let mut max_seq = cursor;
            for (seq, v) in g.ring.iter() {
                if *seq > cursor {
                    out.push(v.clone());
                    if *seq > max_seq {
                        max_seq = *seq;
                    }
                }
            }
            // If nothing matched but the ring has data, advance the cursor
            // to the latest known seq so the client's next poll is fresh.
            if out.is_empty()
                && let Some(&(seq, _)) = g.ring.back()
                && seq > max_seq
            {
                max_seq = seq;
            }
            (max_seq, out)
        }
    }
}

// ---------------------------------------------------------------------------
// Event type

/// One control-plane event. Wire-stable: clients switch on `kind`.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub ts_unix: i64,
    pub kind: EventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domid: Option<u32>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum EventKind {
    /// Engine successfully applied a balloon move.
    EngineApply,
    /// Engine planned a move but skipped (cooldown, no-op, or actor error).
    EngineSkip,
    /// Engine couldn't reach libxl this tick.
    BackendStateChange,
    /// Engine auto-inserted a default policy for a freshly-seen domain.
    /// Emitted once per domain per daemon run; user-set policies are
    /// never overwritten.
    EnginePolicyAutoDefault,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Engine commands + status

pub enum EngineCmd {
    Status { reply: oneshot::Sender<EngineStatus> },
    Shutdown,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub running: bool,
    pub last_tick_unix: i64,
    pub controlled_domains: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Direction marker for cooldown bookkeeping.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Direction { Up, Down }

// ---------------------------------------------------------------------------
// Handle

#[derive(Clone)]
pub struct EngineHandle {
    tx: Sender<EngineCmd>,
    policies: Arc<RwLock<HashMap<u32, PolicyMemory>>>,
    events: broadcast::Sender<Event>,
    last_tick: Arc<Mutex<i64>>,
}

impl EngineHandle {
    /// Synchronous status snapshot. Implemented as a direct read of the
    /// shared state rather than a round-trip through the engine thread,
    /// because `controlled_domains` is just the policy map and a tick
    /// timestamp; the engine thread is busy sleeping.
    pub fn status(&self) -> EngineStatus {
        let policies = self.policies.read().expect("policies poisoned");
        let mut domains: Vec<u32> = policies.keys().copied().collect();
        domains.sort_unstable();
        let last_tick_unix = *self.last_tick.lock().expect("last_tick poisoned");
        EngineStatus {
            running: true,
            last_tick_unix,
            controlled_domains: domains,
        }
    }

    pub fn set_policy(&self, domid: u32, policy: PolicyMemory) {
        let mut g = self.policies.write().expect("policies poisoned");
        g.insert(domid, policy);
    }

    /// Subscribe to the event bus. Receivers share the underlying ring;
    /// `drain_since(cursor)` is the only way to read.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Best-effort shutdown: signals the tick thread to exit.
    pub fn shutdown(&self) {
        let _ = self.tx.send(EngineCmd::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// Spawn + tick loop

/// Spawn the engine thread. Caller keeps the returned handle for the
/// daemon's lifetime and clones it into per-connection state.
pub fn start(actor: ActorHandle) -> EngineHandle {
    let (tx, rx) = mpsc::channel::<EngineCmd>();
    let policies: Arc<RwLock<HashMap<u32, PolicyMemory>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let (ev_tx, _ev_rx) = broadcast::channel::<Event>();
    let last_tick = Arc::new(Mutex::new(0i64));

    let policies_thread = policies.clone();
    let ev_thread = ev_tx.clone();
    let last_tick_thread = last_tick.clone();

    thread::Builder::new()
        .name("orchestratord-engine".into())
        .spawn(move || engine_loop(actor, rx, policies_thread, ev_thread, last_tick_thread))
        .expect("spawn orchestratord-engine");

    EngineHandle { tx, policies, events: ev_tx, last_tick }
}

fn engine_loop(
    actor: ActorHandle,
    rx: std::sync::mpsc::Receiver<EngineCmd>,
    policies: Arc<RwLock<HashMap<u32, PolicyMemory>>>,
    events: broadcast::Sender<Event>,
    last_tick: Arc<Mutex<i64>>,
) {
    // (domid, dir) → wallclock Instant of the last apply in that direction.
    let mut cooldowns: HashMap<(u32, Direction), Instant> = HashMap::new();
    // Sticky flag so we only emit BackendStateChange on edges, not every
    // tick the backend stays unavailable.
    let mut backend_was_available = true;

    let tick = Duration::from_secs(1);

    loop {
        // Drain all queued commands first. If Shutdown shows up at any
        // point we exit immediately.
        loop {
            match rx.try_recv() {
                Ok(EngineCmd::Shutdown) => return,
                Ok(EngineCmd::Status { reply }) => {
                    let policies_g = policies.read().expect("policies poisoned");
                    let mut domains: Vec<u32> = policies_g.keys().copied().collect();
                    domains.sort_unstable();
                    let last = *last_tick.lock().expect("last_tick poisoned");
                    let _ = reply.send(EngineStatus {
                        running: true,
                        last_tick_unix: last,
                        controlled_domains: domains,
                    });
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }

        // ---- one tick ------------------------------------------------------
        run_tick(
            &actor,
            &policies,
            &events,
            &mut cooldowns,
            &mut backend_was_available,
            &last_tick,
        );

        // Sleep but stay responsive to Shutdown — a 1 s recv_timeout wakes
        // if the channel got something while we were sleeping.
        match rx.recv_timeout(tick) {
            Ok(EngineCmd::Shutdown) => return,
            Ok(EngineCmd::Status { reply }) => {
                let policies_g = policies.read().expect("policies poisoned");
                let mut domains: Vec<u32> = policies_g.keys().copied().collect();
                domains.sort_unstable();
                let last = *last_tick.lock().expect("last_tick poisoned");
                let _ = reply.send(EngineStatus {
                    running: true,
                    last_tick_unix: last,
                    controlled_domains: domains,
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn run_tick(
    actor: &ActorHandle,
    policies: &RwLock<HashMap<u32, PolicyMemory>>,
    events: &broadcast::Sender<Event>,
    cooldowns: &mut HashMap<(u32, Direction), Instant>,
    backend_was_available: &mut bool,
    last_tick: &Mutex<i64>,
) {
    *last_tick.lock().expect("last_tick poisoned") = now_unix();

    // We always poll domain_list now — the engine auto-inserts a default
    // policy the first time it sees a domain, so the user doesn't have
    // to call engine.set_policy by hand to get the rebalancer doing
    // anything. The "no policies" early-return that used to live here
    // moved to: if domain_list fails (no backend), skip silently.
    let domains = match actor.domain_list() {
        Ok(d) => {
            if !*backend_was_available {
                events.send(Event {
                    ts_unix: now_unix(),
                    kind: EventKind::BackendStateChange,
                    domid: None,
                    message: "backend available".into(),
                });
                *backend_was_available = true;
            }
            d
        }
        Err(ActorError::BackendUnavailable(_)) => {
            if *backend_was_available {
                events.send(Event {
                    ts_unix: now_unix(),
                    kind: EventKind::BackendStateChange,
                    domid: None,
                    message: "backend unavailable".into(),
                });
                *backend_was_available = false;
            }
            return;
        }
        Err(e) => {
            events.send(Event {
                ts_unix: now_unix(),
                kind: EventKind::EngineSkip,
                domid: None,
                message: format!("domain_list failed: {e:?}"),
            });
            return;
        }
    };

    // Auto-default pass: insert a sensible policy for every domain we
    // haven't seen before. Done in a single write-lock over the map so
    // user-set policies (via engine.set_policy) aren't clobbered.
    // Manifest scanning to honour `[policy.memory]` will land with the
    // image catalog; v0.0.4 uses libxl-derived defaults only.
    let inserts: Vec<(u32, PolicyMemory, String)> = {
        let mut g = policies.write().expect("policies poisoned");
        let mut new_inserts = Vec::new();
        for d in &domains {
            if g.contains_key(&d.domid) {
                continue;
            }
            let p = default_policy_for(d.domid, d.memory_mb, d.memory_max_mb);
            let msg = format!(
                "auto-applied default policy: min={} max={}",
                p.min_mb.unwrap_or(0), p.max_mb.unwrap_or(0),
            );
            g.insert(d.domid, p.clone());
            new_inserts.push((d.domid, p, msg));
        }
        new_inserts
    };
    for (domid, _policy, message) in inserts {
        events.send(Event {
            ts_unix: now_unix(),
            kind: EventKind::EnginePolicyAutoDefault,
            domid: Some(domid),
            message,
        });
    }

    let snapshot: HashMap<u32, PolicyMemory> = {
        let g = policies.read().expect("policies poisoned");
        g.clone()
    };

    let now = Instant::now();

    for d in domains {
        let policy = match snapshot.get(&d.domid) {
            Some(p) => p.clone(),
            None => continue,
        };

        let plan = plan_move(d.memory_mb, &policy);
        let (target_mb, dir) = match plan {
            Some(x) => x,
            None => continue,
        };

        // Cooldown: skip if we acted in the same direction recently.
        if let Some(cd_s) = policy.cooldown_s
            && let Some(last) = cooldowns.get(&(d.domid, dir))
            && now.duration_since(*last) < Duration::from_secs(cd_s)
        {
            events.send(Event {
                ts_unix: now_unix(),
                kind: EventKind::EngineSkip,
                domid: Some(d.domid),
                message: format!("cooldown active for {dir:?}"),
            });
            continue;
        }

        // Apply. Convert MB → kB on the wire (the actor's contract).
        let target_kb = target_mb.saturating_mul(1024);
        match actor.domain_balloon(d.domid, target_kb) {
            Ok(()) => {
                cooldowns.insert((d.domid, dir), now);
                events.send(Event {
                    ts_unix: now_unix(),
                    kind: EventKind::EngineApply,
                    domid: Some(d.domid),
                    message: format!(
                        "balloon {dir:?} from {} MB → {} MB",
                        d.memory_mb, target_mb
                    ),
                });
            }
            Err(e) => {
                events.send(Event {
                    ts_unix: now_unix(),
                    kind: EventKind::EngineSkip,
                    domid: Some(d.domid),
                    message: format!("balloon failed: {e:?}"),
                });
            }
        }
    }
}

/// Per-domain default policy used the first time the engine sees a
/// domain. dom0 gets a no-op policy (min == max == current) so we
/// don't shrink the host out from under the user — opt-in via the
/// cockpit's `[M]` editor. domU defaults pin the ceiling to the
/// libxl-reported max and leave the floor at 0; that's defensive but
/// safe (the engine won't try to grow above max_memkb).
fn default_policy_for(domid: u32, current_mb: u64, max_mb: u64) -> PolicyMemory {
    if domid == 0 {
        PolicyMemory {
            min_mb: Some(current_mb),
            max_mb: Some(current_mb),
            target_headroom_pct: None,
            cooldown_s: Some(60),
        }
    } else {
        PolicyMemory {
            min_mb: Some(0),
            max_mb: Some(max_mb),
            target_headroom_pct: None,
            cooldown_s: Some(30),
        }
    }
}

/// Decide a single balloon move per tick. Floor/ceiling only — headroom
/// logic is reserved for v1. Returns `(target_mb, direction)`.
fn plan_move(current_mb: u64, policy: &PolicyMemory) -> Option<(u64, Direction)> {
    if let Some(max) = policy.max_mb
        && current_mb > max
    {
        return Some((max, Direction::Down));
    }
    if let Some(min) = policy.min_mb
        && current_mb < min
    {
        return Some((min, Direction::Up));
    }
    None
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_send_increments_seq_monotonically() {
        let (tx, rx) = broadcast::channel::<u32>();
        let s1 = tx.send(10);
        let s2 = tx.send(20);
        let s3 = tx.send(30);
        assert!(s2 > s1);
        assert!(s3 > s2);
        let (cursor, items) = rx.drain_since(0);
        assert_eq!(items, vec![10, 20, 30]);
        assert_eq!(cursor, s3);
    }

    #[test]
    fn broadcast_drain_since_returns_only_newer() {
        let (tx, rx) = broadcast::channel::<u32>();
        tx.send(1);
        let s2 = tx.send(2);
        tx.send(3);
        let (cursor, items) = rx.drain_since(s2);
        assert_eq!(items, vec![3]);
        assert!(cursor >= s2);
    }

    #[test]
    fn broadcast_drops_oldest_at_capacity_boundary() {
        let (tx, rx) = broadcast::channel::<u64>();
        // 1025 events: oldest one drops, ring holds 1024.
        for i in 0..(broadcast::CAPACITY as u64 + 1) {
            tx.send(i);
        }
        let (cursor, items) = rx.drain_since(0);
        assert_eq!(items.len(), broadcast::CAPACITY);
        // First surviving value is i=1, last is i=1024.
        assert_eq!(*items.first().unwrap(), 1);
        assert_eq!(*items.last().unwrap(), broadcast::CAPACITY as u64);
        // Cursor reflects the highest seq actually present.
        assert_eq!(cursor, broadcast::CAPACITY as u64 + 1);
    }

    #[test]
    fn plan_move_shrinks_when_above_ceiling() {
        let p = PolicyMemory { max_mb: Some(2048), ..Default::default() };
        assert_eq!(plan_move(4096, &p), Some((2048, Direction::Down)));
        assert_eq!(plan_move(1024, &p), None);
    }

    #[test]
    fn plan_move_grows_when_below_floor() {
        let p = PolicyMemory { min_mb: Some(512), ..Default::default() };
        assert_eq!(plan_move(256, &p), Some((512, Direction::Up)));
        assert_eq!(plan_move(1024, &p), None);
    }

    #[test]
    fn plan_move_no_action_inside_band() {
        let p = PolicyMemory {
            min_mb: Some(256),
            max_mb: Some(2048),
            ..Default::default()
        };
        assert_eq!(plan_move(1024, &p), None);
    }

    #[test]
    fn default_policy_for_dom0_is_noop_at_current() {
        // dom0 default: min == max == current_mb so the planner finds
        // no move. Spelled out so a future change to the helper that
        // would un-pin dom0 fails this test loudly.
        let p = default_policy_for(0, 4096, 8192);
        assert_eq!(p.min_mb, Some(4096));
        assert_eq!(p.max_mb, Some(4096));
        assert_eq!(plan_move(4096, &p), None);
    }

    #[test]
    fn default_policy_for_domu_caps_at_libxl_max() {
        let p = default_policy_for(7, 1024, 4096);
        assert_eq!(p.min_mb, Some(0));
        assert_eq!(p.max_mb, Some(4096));
        // current under max → no shrink; current over max → would shrink.
        assert_eq!(plan_move(1024, &p), None);
        assert_eq!(plan_move(8192, &p), Some((4096, Direction::Down)));
    }

    #[test]
    fn engine_status_reflects_set_policy() {
        let actor = crate::actor::spawn();
        let engine = start(actor.clone());
        engine.set_policy(7, PolicyMemory {
            min_mb: Some(256), max_mb: Some(2048), ..Default::default()
        });
        let s = engine.status();
        assert!(s.controlled_domains.contains(&7));
        engine.shutdown();
        actor.shutdown();
    }
}
