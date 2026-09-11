//! Native binding state. No session method acquires the registry; callers
//! needing both locks take the registry first. Authority transitions and route
//! installation happen in that critical section.
use super::*;
use cosmix_bus::native_session::*;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};

const LEASE_MS: u64 = 15_000;
const RESUME_MS: u64 = 30_000;
pub(super) const MAX_ISSUED: usize = 65_536;
type Id = HexBytes<16>;
type Reply = Result<serde_json::Value, SessionError>;

pub(crate) fn now_ms() -> Result<u64, SessionError> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid writable timespec. Failure must not mint authority.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return Err(error(ErrorCode::Unavailable, "clock_unavailable"));
    }
    Ok((ts.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add(ts.tv_nsec as u64 / 1_000_000))
}

fn error(code: ErrorCode, reason: &str) -> SessionError {
    let mut details = serde_json::Map::new();
    if !reason.is_empty() {
        details.insert("reason".into(), reason.into());
    }
    SessionError {
        error_code: code,
        message: "session request refused".into(),
        details,
    }
}

/// Owned by the broker serve future, so aborting a broker also stops its timer.
pub(super) struct MaintenanceTask(tokio::task::JoinHandle<()>);

impl Drop for MaintenanceTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) fn deadline_timer() -> std::io::Result<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    // SAFETY: timerfd_create returns a new owned descriptor on success.
    let fd = unsafe {
        libc::timerfd_create(libc::CLOCK_BOOTTIME, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC)
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: this is the sole owner of the newly created descriptor.
    tokio::io::unix::AsyncFd::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Sleep until an absolute BOOTTIME deadline, including time spent suspended.
/// Tokio's ordinary Instant timer uses CLOCK_MONOTONIC on Linux instead.
async fn sleep_until(
    timer: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    deadline: u64,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let deadline = deadline.max(1); // zero would disarm timerfd
    let spec = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: (deadline / 1000) as libc::time_t,
            tv_nsec: ((deadline % 1000) * 1_000_000) as libc::c_long,
        },
    };
    // SAFETY: valid timer descriptor and input timespec; no old-value output.
    if unsafe {
        libc::timerfd_settime(
            timer.get_ref().as_raw_fd(),
            libc::TFD_TIMER_ABSTIME,
            &spec,
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    loop {
        let mut ready = timer.readable().await?;
        match ready.try_io(|fd| {
            let mut expirations = 0u64;
            // SAFETY: valid descriptor and writable buffer of exactly 8 bytes.
            let count = unsafe {
                libc::read(
                    fd.get_ref().as_raw_fd(),
                    (&mut expirations as *mut u64).cast(),
                    8,
                )
            };
            if count < 0 {
                Err(std::io::Error::last_os_error())
            } else if count == 8 {
                Ok(())
            } else {
                Err(std::io::Error::other("short timer read"))
            }
        }) {
            Ok(result) => return result,
            Err(_) => continue, // readiness raced with re-arming; await the FD
        }
    }
}

pub(super) fn spawn_maintenance(
    registry: Arc<RwLock<HashMap<String, ServiceEntry>>>,
    sessions: Arc<tokio::sync::Mutex<Sessions>>,
    timer: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
) -> MaintenanceTask {
    MaintenanceTask(tokio::spawn(async move {
        let wake = sessions.lock().await.deadline_wake.clone();
        loop {
            let (failed, deadline) = {
                let sessions = sessions.lock().await;
                (sessions.clock_failed, sessions.next_deadline())
            };
            if failed {
                let mut reg = registry.write().await;
                sessions.lock().await.fail_clock(&mut reg);
                return;
            }
            let expiry = async {
                match deadline {
                    Some(deadline) => sleep_until(&timer, deadline).await,
                    None => std::future::pending::<std::io::Result<()>>().await,
                }
            };
            tokio::select! {
                _ = wake.notified() => continue,
                result = expiry => {
                    let mut reg = registry.write().await;
                    let mut sessions = sessions.lock().await;
                    match result.and_then(|()| sessions.checked_now().map_err(|_| std::io::Error::other("clock unavailable"))) {
                        Ok(now) => sessions.maintain(&mut reg, now),
                        Err(_) => {
                            sessions.fail_clock(&mut reg);
                            tracing::error!("Native session expiry clock unavailable");
                            return;
                        }
                    }
                }
            }
        }
    }))
}

fn verify(key: HexBytes<32>, signature: HexBytes<64>, bytes: &[u8]) -> Result<(), SessionError> {
    let key = strict_key(key)?;
    key.verify_strict(bytes, &Signature::from_bytes(&signature.0))
        .map_err(|_| SessionError::forbidden())
}

fn strict_key(bytes: HexBytes<32>) -> Result<VerifyingKey, SessionError> {
    // Compressed Edwards y must be canonical (< 2^255-19), independently
    // of dalek's field decoding. The high bit encodes the x sign.
    let mut y = bytes.0;
    y[31] &= 127;
    let mut prime = [255; 32];
    prime[0] = 237;
    prime[31] = 127;
    if y.iter().rev().cmp(prime.iter().rev()) != std::cmp::Ordering::Less {
        return Err(SessionError::forbidden());
    }
    let key = VerifyingKey::from_bytes(&bytes.0).map_err(|_| SessionError::forbidden())?;
    if key.is_weak() {
        return Err(SessionError::forbidden());
    }
    Ok(key)
}

struct Record {
    sequence: usize,
    parent_id: Option<Id>,
    grant_id: Option<Id>,
    view: SessionRecord,
    key: HexBytes<32>,
    connection: Option<Id>,
    deadline: u64,
}

struct Cached {
    connection: Id,
    id: u64,
    command: SessionCommand,
    result: Reply,
    expires: u64,
    bytes: usize,
}

struct Connection {
    principal: BrokerPrincipal,
    tx: mpsc::Sender<String>,
    close: Arc<tokio::sync::Notify>,
    protected: Arc<AtomicBool>,
    high_water: u64,
    interest: Option<HexBytes<32>>,
    challenge: Option<(ChallengeArgs, ProofTranscript)>,
    consumed: HashSet<Id>,
    binding: Option<Id>,
}

struct Outbox {
    tx: mpsc::Sender<String>,
    wake: Arc<tokio::sync::Notify>,
    notices: VecDeque<String>,
    gap: bool,
    dependencies: Vec<(RecordRef, u64)>,
}

pub(crate) struct Sessions {
    pending_grants_per_parent: usize,
    records: HashMap<Id, Record>,
    connections: HashMap<Id, Connection>,
    issued: HashSet<String>,
    results: VecDeque<Cached>,
    grants: HashMap<Id, SessionGrant>,
    pane_high_water: HashMap<(Id, u64), u64>,
    outboxes: HashMap<Id, Outbox>,
    last_maintained: Option<u64>,
    deadline_wake: Arc<tokio::sync::Notify>,
    clock_failed: bool,
}

impl Default for Sessions {
    fn default() -> Self {
        // This default coincides with Term's MAX_TABS, not an admission promise:
        // its look-ahead grant also spends a slot and operators may lower it.
        Self::with_grant_limit(32)
    }
}

impl Sessions {
    pub(super) fn with_grant_limit(pending_grants_per_parent: usize) -> Self {
        Self {
            pending_grants_per_parent,
            records: Default::default(),
            connections: Default::default(),
            issued: Default::default(),
            results: Default::default(),
            grants: Default::default(),
            pane_high_water: Default::default(),
            outboxes: Default::default(),
            last_maintained: None,
            deadline_wake: Default::default(),
            clock_failed: false,
        }
    }

    pub(super) fn pending_grants_per_parent(&self) -> usize {
        self.pending_grants_per_parent
    }

    /// Exercise the production bounded queues with the writer excluded by the
    /// same lock. No fabricated broker or transport substitutes for delivery.
    #[cfg(test)]
    pub(super) fn test_notice_burst(&mut self, id: Id, count: usize) {
        for _ in 0..count {
            self.notice(id, None);
        }
    }
    pub(super) fn open_outbox(
        &mut self,
        id: Id,
        tx: &mpsc::Sender<String>,
        wake: Arc<tokio::sync::Notify>,
    ) {
        self.outboxes.insert(
            id,
            Outbox {
                tx: tx.clone(),
                wake,
                notices: VecDeque::new(),
                gap: false,
                dependencies: Vec::new(),
            },
        );
    }

    pub(super) fn close_outbox(&mut self, id: Id) {
        self.outboxes.remove(&id);
        self.deadline_wake.notify_one();
    }

    pub(super) fn next_notice(&mut self, id: Id, epoch: Id) -> Option<(bool, String)> {
        let out = self.outboxes.get_mut(&id)?;
        if std::mem::take(&mut out.gap) {
            return Some((
                true,
                BusMessage::new()
                    .with_header("bus", "1")
                    .with_header("type", "event")
                    .with_header("command", "noded.session.lifecycle.gap")
                    .with_body(&serde_json::json!({"broker_epoch":epoch}).to_string())
                    .to_wire(),
            ));
        }
        out.notices.pop_front().map(|wire| (false, wire))
    }

    pub(super) fn restore_gap(&mut self, id: Id) {
        if let Some(out) = self.outboxes.get_mut(&id) {
            out.gap = true;
            out.wake.notify_one();
        }
    }

    fn queue_notice(&mut self, id: Id, wire: String) {
        let Some(out) = self.outboxes.get_mut(&id) else {
            return;
        };
        if out.notices.len() >= 256 {
            out.gap = true;
            out.wake.notify_one();
            return;
        }
        if self
            .outboxes
            .values()
            .map(|o| o.notices.len())
            .sum::<usize>()
            >= 4096
        {
            let largest = *self
                .outboxes
                .iter()
                .max_by_key(|(_, o)| o.notices.len())
                .expect("outbox")
                .0;
            let out = self.outboxes.get_mut(&largest).expect("outbox");
            out.notices.pop_front();
            out.gap = true;
            out.wake.notify_one();
        }
        let out = self.outboxes.get_mut(&id).expect("outbox");
        out.notices.push_back(wire);
        out.wake.notify_one();
    }

    fn notice(&mut self, id: Id, closing: Option<Id>) {
        let r = &self.records[&id];
        let reference = r.view.reference();
        let mut recipients = HashSet::new();
        recipients.extend(closing);
        recipients.extend(r.connection);
        recipients.extend(self.parent(r).and_then(|p| p.connection));
        for (cid, out) in &self.outboxes {
            if out.dependencies.iter().any(|(target, _)| {
                target.record_id == id && target.incarnation == reference.incarnation
            }) {
                recipients.insert(*cid);
            }
        }
        for (cid, c) in &self.connections {
            if c.principal.unix_uid == r.view.owner_uid && c.interest == Some(r.key) {
                recipients.insert(*cid);
            }
        }
        let wire = BusMessage::new().with_header("bus","1").with_header("type","event").with_header("command","noded.session.lifecycle").with_body(&serde_json::json!({"target":reference,"state":r.view.state,"broker_epoch":r.view.broker_epoch}).to_string()).to_wire();
        for cid in recipients {
            self.queue_notice(cid, wire.clone());
        }
    }

    pub(crate) fn delivery_now(
        &mut self,
        p: &BrokerPrincipal,
        target: &mpsc::Sender<String>,
    ) -> Result<Option<BrokerPrincipal>, SessionError> {
        let now = self.checked_now()?;
        self.delivery(p, target, now)
    }

    pub(crate) fn publisher_now(
        &mut self,
        p: &BrokerPrincipal,
    ) -> Result<Option<BrokerPrincipal>, SessionError> {
        let now = self.checked_now()?;
        self.publisher(p, now)
    }

    fn publisher(
        &self,
        p: &BrokerPrincipal,
        now: u64,
    ) -> Result<Option<BrokerPrincipal>, SessionError> {
        if self.clock_failed {
            return Err(error(ErrorCode::Unavailable, "clock_unavailable"));
        }
        match self.attached(p.connection_id) {
            Some(id) if self.remaining(&self.records[&id], now) == 0 => {
                Err(error(ErrorCode::Expired, ""))
            }
            None if p.session.is_some() => Err(error(ErrorCode::Expired, "")),
            _ => Ok(self.principal(p.connection_id, now)),
        }
    }

    /// A read-only route lookup must still refuse an expired native target
    /// while the scheduler is waiting to acquire the registry write lock.
    pub(super) fn validate_route(&mut self, name: &str) -> Result<(), SessionError> {
        let now = self.checked_now()?;
        if self
            .records
            .values()
            .find(|r| r.view.name == name)
            .is_some_and(|r| self.remaining(r, now) == 0)
        {
            return Err(error(ErrorCode::Expired, ""));
        }
        Ok(())
    }

    pub(crate) fn delivery(
        &mut self,
        p: &BrokerPrincipal,
        target: &mpsc::Sender<String>,
        now: u64,
    ) -> Result<Option<BrokerPrincipal>, SessionError> {
        let principal = self.publisher(p, now)?;
        let Some(id) = self.attached(p.connection_id) else {
            return Ok(principal);
        };
        let r = &self.records[&id];
        let remaining = self.remaining(r, now);
        let reference = r.view.reference();
        // Topic fan-out reaches this admission path without a registry sweep.
        // Expired dependencies must not occupy either quota until an expiry wake.
        for out in self.outboxes.values_mut() {
            out.dependencies.retain(|(_, expires)| *expires > now);
        }
        let global = self
            .outboxes
            .values()
            .map(|o| o.dependencies.len())
            .sum::<usize>();
        let out = self
            .outboxes
            .values_mut()
            .find(|o| o.tx.same_channel(target))
            .ok_or_else(|| error(ErrorCode::Unavailable, "recipient_missing"))?;
        if let Some((_, expires)) = out.dependencies.iter_mut().find(|(r, _)| *r == reference) {
            *expires = now + remaining;
        } else {
            if out.dependencies.len() >= 256 || global >= 8192 {
                return Err(error(
                    ErrorCode::ResourceLimit,
                    "recipient_dependency_limit",
                ));
            }
            out.dependencies.push((reference, now + remaining));
        }
        self.deadline_wake.notify_one();
        Ok(principal)
    }
    pub(super) fn connect(
        &mut self,
        principal: &BrokerPrincipal,
        tx: &mpsc::Sender<String>,
        close: Arc<tokio::sync::Notify>,
        protected: Arc<AtomicBool>,
    ) {
        self.connections.insert(
            principal.connection_id,
            Connection {
                principal: principal.clone(),
                tx: tx.clone(),
                close,
                protected,
                high_water: 0,
                interest: None,
                challenge: None,
                consumed: HashSet::new(),
                binding: None,
            },
        );
    }

    fn snapshot(&self, record: &Record, now: u64) -> SessionRecord {
        let mut view = record.view.clone();
        view.lease_remaining_ms =
            (view.state == BindingState::Attached).then(|| DecimalU64(self.remaining(record, now)));
        view
    }

    fn parent(&self, r: &Record) -> Option<&Record> {
        let p = self.records.get(&r.parent_id?)?;
        (Some(p.view.instance_id) == r.view.parent_instance
            && Some(p.view.incarnation) == r.view.parent_incarnation)
            .then_some(p)
    }

    fn remaining(&self, r: &Record, now: u64) -> u64 {
        if r.view.state != BindingState::Attached {
            return 0;
        }
        let own = r.deadline.saturating_sub(now);
        match self.parent(r) {
            Some(p) => own.min(self.remaining(p, now)),
            None if r.view.role == Role::Term => own,
            None => 0,
        }
    }

    fn parent_live(&self, r: &Record, now: u64) -> bool {
        r.view.role == Role::Term || self.parent(r).is_some_and(|p| self.remaining(p, now) > 0)
    }

    fn owned(&self, uid: u32, target: &RecordRef) -> Result<&Record, SessionError> {
        let r = self
            .records
            .get(&target.record_id)
            .filter(|r| r.view.owner_uid == uid)
            .ok_or_else(SessionError::forbidden)?;
        if r.view.reference() != *target {
            return Err(error(ErrorCode::StaleGeneration, ""));
        }
        Ok(r)
    }

    fn children(&self, id: Id) -> Vec<Id> {
        let p = &self.records[&id].view;
        if p.role != Role::Term {
            return Vec::new();
        }
        self.records
            .iter()
            .filter(|(_, r)| {
                r.view.parent_instance == Some(p.instance_id)
                    && r.view.parent_incarnation == Some(p.incarnation)
            })
            .map(|(id, _)| *id)
            .collect()
    }

    fn revoke(&mut self, id: Id, reg: &mut HashMap<String, ServiceEntry>) {
        for child in self.children(id) {
            self.revoke(child, reg);
        }
        let r = self.records.get_mut(&id).expect("record");
        if r.view.state == BindingState::Revoked {
            return;
        }
        let closing = r.connection;
        if let Some(c) = r.connection.take().and_then(|id| self.connections.get(&id)) {
            if reg.get(&r.view.name).is_some_and(|e| e.same_channel(&c.tx)) {
                reg.remove(&r.view.name);
            }
            c.close.notify_one();
        }
        r.view.state = BindingState::Revoked;
        if let Some(g) = r.grant_id.and_then(|id| self.grants.get_mut(&id))
            && g.state == GrantState::Pending
        {
            g.state = GrantState::Revoked;
        }
        self.notice(id, closing);
    }

    fn attached(&self, connection: Id) -> Option<Id> {
        self.records
            .iter()
            .find_map(|(id, r)| (r.connection == Some(connection)).then_some(*id))
    }

    fn issue_name(
        &mut self,
        uid: u32,
        child: bool,
        reg: &HashMap<String, ServiceEntry>,
    ) -> Result<String, SessionError> {
        if self.issued.len() >= MAX_ISSUED {
            return Err(error(ErrorCode::ResourceLimit, "issued_name_limit"));
        }
        let mut n = uid;
        let mut digits = Vec::new();
        loop {
            digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize] as char);
            n /= 36;
            if n == 0 {
                break;
            }
        }
        let uid: String = digits.iter().rev().collect();
        for _ in 0..9 {
            let suffix: String = (0..22)
                .map(|_| {
                    b"abcdefghijklmnopqrstuvwxyz234567"[(rand::random::<u8>() & 31) as usize]
                        as char
                })
                .collect();
            let name = format!("{}{uid}-{suffix}", if child { 'c' } else { 't' });
            if !reg.contains_key(&name) && self.issued.insert(name.clone()) {
                return Ok(name);
            }
        }
        Err(error(ErrorCode::ResourceLimit, "name_collision"))
    }

    fn install(&self, id: Id, reg: &mut HashMap<String, ServiceEntry>) {
        let r = &self.records[&id];
        let c = &self.connections[&r.connection.expect("attached")];
        reg.retain(|_, entry| !entry.same_channel(&c.tx));
        reg.insert(
            r.view.name.clone(),
            ServiceEntry {
                protected_responses: c.protected.clone(),
                traffic_class: TrafficClass::NativeSession,
                tx: c.tx.clone(),
                info: cosmix_bus::ServiceInfo::from_name(&r.view.name),
            },
        );
    }

    fn suspend(&mut self, id: Id, reg: &mut HashMap<String, ServiceEntry>, now: u64) {
        for child in self.children(id) {
            self.suspend(child, reg, now);
        }
        let r = self.records.get_mut(&id).expect("record");
        if r.view.state != BindingState::Attached {
            return;
        }
        let closing = r.connection;
        if let Some(c) = r.connection.take().and_then(|id| self.connections.get(&id)) {
            if reg.get(&r.view.name).is_some_and(|e| e.same_channel(&c.tx)) {
                reg.remove(&r.view.name);
            }
            c.close.notify_one();
        }
        r.view.state = BindingState::Suspended;
        r.deadline = now.saturating_add(RESUME_MS);
        self.notice(id, closing);
    }

    fn checked_now(&mut self) -> Result<u64, SessionError> {
        if self.clock_failed {
            return Err(error(ErrorCode::Unavailable, "clock_unavailable"));
        }
        now_ms().inspect_err(|_| {
            self.clock_failed = true;
            self.deadline_wake.notify_one();
        })
    }

    fn fail_clock(&mut self, reg: &mut HashMap<String, ServiceEntry>) {
        // Deliberately sticky until broker restart: clock recovery cannot revive authority.
        self.clock_failed = true;
        for id in self.records.keys().copied().collect::<Vec<_>>() {
            self.revoke(id, reg);
        }
        for c in self.connections.values() {
            c.close.notify_one();
        }
    }

    pub(super) fn maintain_now(
        &mut self,
        reg: &mut HashMap<String, ServiceEntry>,
    ) -> Result<u64, SessionError> {
        let now = match self.checked_now() {
            Ok(now) => now,
            Err(error) => {
                self.fail_clock(reg);
                return Err(error);
            }
        };
        self.maintain(reg, now);
        self.deadline_wake.notify_one();
        Ok(now)
    }

    /// One absolute BOOTTIME deadline for pure-expiry work; no periodic sweep.
    /// O(n) over bounded epoch state, cheaper than per-connection periodic sweeps.
    fn next_deadline(&self) -> Option<u64> {
        self.records
            .values()
            .filter(|r| {
                matches!(
                    r.view.state,
                    BindingState::Attached | BindingState::Suspended
                )
            })
            .map(|r| r.deadline)
            .chain(
                self.grants
                    .values()
                    .filter(|g| g.state == GrantState::Pending)
                    .map(|g| g.expires_ms.0),
            )
            .chain(
                self.connections
                    .values()
                    .filter_map(|c| c.challenge.as_ref().map(|(_, p)| p.challenge_expires_ms.0)),
            )
            .chain(self.results.iter().map(|r| r.expires))
            .chain(
                self.outboxes
                    .values()
                    .flat_map(|o| o.dependencies.iter().map(|(_, expires)| *expires)),
            )
            .min()
    }

    pub(super) fn maintain(&mut self, reg: &mut HashMap<String, ServiceEntry>, now: u64) {
        // Explicit time supports synthetic-clock regressions; live callers use maintain_now or the scheduler.
        self.last_maintained = Some(now);
        let mut expired: Vec<_> = self
            .records
            .iter()
            .filter(|(_, r)| {
                r.deadline <= now
                    && matches!(
                        r.view.state,
                        BindingState::Attached | BindingState::Suspended
                    )
            })
            .map(|(id, r)| (*id, r.view.state, r.deadline))
            .collect();
        expired.sort_by_key(|(_, _, deadline)| *deadline);
        for (id, state, deadline) in expired {
            if state == BindingState::Attached {
                self.suspend(id, reg, deadline);
                if self.records[&id].deadline <= now {
                    self.revoke(id, reg);
                }
            } else {
                self.revoke(id, reg);
            }
        }
        self.results.retain(|r| r.expires > now);
        for out in self.outboxes.values_mut() {
            out.dependencies.retain(|(_, expires)| *expires > now);
        }
        let expired: Vec<_> = self
            .grants
            .values_mut()
            .filter(|g| g.state == GrantState::Pending && g.expires_ms.0 <= now)
            .map(|g| {
                g.state = GrantState::Expired;
                g.record_id
            })
            .collect();
        for id in expired {
            self.revoke(id, reg);
        }
        for c in self.connections.values_mut() {
            if c.challenge
                .as_ref()
                .is_some_and(|(_, p)| p.challenge_expires_ms.0 <= now)
            {
                c.challenge = None;
            }
        }
    }

    pub(super) fn disconnect(&mut self, id: Id, reg: &mut HashMap<String, ServiceEntry>) {
        if let Ok(now) = self.maintain_now(reg)
            && let Some(record) = self.attached(id)
        {
            self.suspend(record, reg, now);
        }
        self.connections.remove(&id);
        self.results.retain(|r| r.connection != id);
    }

    pub(super) fn name(&self, connection: Id) -> Option<String> {
        self.attached(connection)
            .map(|id| self.records[&id].view.name.clone())
    }

    pub(super) fn discovery(
        &self,
        name: &str,
        uid: Option<u32>,
        now: u64,
    ) -> Option<serde_json::Value> {
        let r = self.records.values().find(|r| r.view.name == name)?;
        if uid != Some(r.view.owner_uid) {
            return Some(serde_json::Value::String(name.into()));
        }
        let mut info = cosmix_bus::ServiceInfo::from_name(name);
        info.native_session = Some(self.snapshot(r, now));
        Some(serde_json::to_value(info).expect("discovery"))
    }

    pub(super) fn discovery_names(&self) -> impl Iterator<Item = &str> {
        self.records
            .values()
            .filter(|r| r.view.state != BindingState::Revoked)
            .map(|r| r.view.name.as_str())
    }

    pub(super) fn principal(&self, connection: Id, now: u64) -> Option<BrokerPrincipal> {
        let c = self.connections.get(&connection)?;
        if c.binding.is_some() && self.attached(connection).is_none() {
            return None;
        }
        let mut p = c.principal.clone();
        if let Some(id) = self.attached(connection) {
            let r = &self.records[&id];
            let v = &r.view;
            p.assurance = Assurance::SessionBound;
            p.session = Some(SessionIdentity {
                record_id: id,
                instance_id: v.instance_id,
                incarnation: v.incarnation,
                role: v.role,
                parent_instance: v.parent_instance,
                parent_incarnation: v.parent_incarnation,
                pane_id: v.pane_id,
                pane_generation: v.pane_generation,
                binding_generation: v.binding_generation,
                capabilities: v.capabilities.clone(),
                lease_remaining_ms: DecimalU64(self.remaining(r, now)),
            });
        }
        Some(p)
    }

    pub(super) fn execute(
        &mut self,
        p: &BrokerPrincipal,
        request: &BootstrapRequest,
        reg: &mut HashMap<String, ServiceEntry>,
    ) -> Reply {
        let now = self.maintain_now(reg)?;
        let cid = p.connection_id;
        let id = request
            .message
            .get("id")
            .expect("validated")
            .parse::<u64>()
            .unwrap_or(0);
        if request.command.retained_mutation() {
            if let Some(cached) = self
                .results
                .iter()
                .find(|r| r.connection == cid && r.id == id)
            {
                return if cached.command == request.command {
                    cached.result.clone()
                } else {
                    Err(error(ErrorCode::Conflict, "request_mismatch"))
                };
            }
            if id <= self.connections[&cid].high_water {
                return Err(error(ErrorCode::Conflict, "unknown_outcome"));
            }
        }
        let result = self.dispatch(p, &request.command, reg, now);
        if request.command.retained_mutation() {
            self.connections
                .get_mut(&cid)
                .expect("connection")
                .high_water = id;
            let bytes =
                serde_json::to_vec(&result).expect("result").len() + request.message.body.len();
            while self.results.iter().filter(|r| r.connection == cid).count() >= 1024 {
                let index = self
                    .results
                    .iter()
                    .position(|r| r.connection == cid)
                    .expect("cached");
                self.results.remove(index);
            }
            while !self.results.is_empty()
                && (self.results.len() >= 8192
                    || self
                        .results
                        .iter()
                        .map(|r| r.bytes)
                        .sum::<usize>()
                        .saturating_add(bytes)
                        > 16 * 1024 * 1024)
            {
                self.results.pop_front();
            }
            // Preserve high-water when even an empty cache cannot fit this
            // result: retries report unknown_outcome, never re-execute it.
            if bytes > 16 * 1024 * 1024 {
                return result;
            }
            self.results.push_back(Cached {
                connection: cid,
                id,
                command: request.command.clone(),
                result: result.clone(),
                expires: now + 900_000,
                bytes,
            });
        }
        result
    }

    fn dispatch(
        &mut self,
        p: &BrokerPrincipal,
        command: &SessionCommand,
        reg: &mut HashMap<String, ServiceEntry>,
        now: u64,
    ) -> Reply {
        self.deadline_wake.notify_one();
        if self.clock_failed {
            return Err(error(ErrorCode::Unavailable, "clock_unavailable"));
        }
        if self.last_maintained != Some(now) {
            self.maintain(reg, now);
        }
        match command {
            SessionCommand::GrantCreate(a) => self.grant_create(p, a, reg, now),
            SessionCommand::GrantFetch(a) => {
                let r = self
                    .records
                    .values()
                    .filter(|r| {
                        r.key == a.public_key
                            && r.view.owner_uid == p.unix_uid
                            && self
                                .parent(r)
                                .is_some_and(|parent| parent.connection == Some(p.connection_id))
                    })
                    .max_by_key(|r| r.sequence)
                    .ok_or_else(SessionError::forbidden)?;
                if !self.parent_live(r, now) {
                    return Err(SessionError::forbidden());
                }
                let g = r
                    .grant_id
                    .and_then(|id| self.grants.get(&id))
                    .ok_or_else(SessionError::forbidden)?;
                Ok(serde_json::json!({"grant":g,"record":self.snapshot(r,now)}))
            }
            SessionCommand::Challenge(a) => self.challenge(p, a, now),
            SessionCommand::Prove(a) => self.prove(p, a, reg, now),
            SessionCommand::LeaseCheck(a) => {
                let r = self.owned(p.unix_uid, &a.target)?;
                let remaining = self.remaining(r, now);
                if remaining == 0 {
                    return Err(error(ErrorCode::Expired, ""));
                }
                let out = self
                    .outboxes
                    .get_mut(&p.connection_id)
                    .ok_or_else(SessionError::forbidden)?;
                let Some((_, expires)) = out
                    .dependencies
                    .iter_mut()
                    .find(|(target, expires)| *target == a.target && *expires > now)
                else {
                    return Err(error(ErrorCode::Conflict, "dependency_missing"));
                };
                *expires = now + remaining;
                Ok(serde_json::json!({"lease_remaining_ms":DecimalU64(remaining)}))
            }
            SessionCommand::Revoke(a) => {
                let r = self.owned(p.unix_uid, &a.target)?;
                let owner = r.connection == Some(p.connection_id)
                    || self.parent(r).is_some_and(|parent| {
                        parent.connection == Some(p.connection_id)
                            && self.remaining(parent, now) > 0
                    });
                if !owner {
                    return Err(SessionError::forbidden());
                }
                let revoked = r.view.state != BindingState::Revoked;
                self.revoke(a.target.record_id, reg);
                Ok(serde_json::json!({"revoked":revoked}))
            }
            SessionCommand::Hello => Ok(
                serde_json::json!({"broker_epoch": p.broker_epoch, "connection_id": p.connection_id}),
            ),
            SessionCommand::Allocate(a) => {
                if self.connections[&p.connection_id].binding.is_some() {
                    return Err(error(ErrorCode::Conflict, "already_bound"));
                }
                verify(
                    a.public_key,
                    a.signature,
                    &encode_allocate(p.broker_epoch, p.connection_id, a.public_key, a.policy),
                )?;
                if self.records.values().any(|r| {
                    r.view.owner_uid == p.unix_uid
                        && r.key == a.public_key
                        && r.view.state != BindingState::Revoked
                }) {
                    return Err(error(ErrorCode::Conflict, "key_in_use"));
                }
                if self
                    .records
                    .values()
                    .filter(|r| {
                        r.view.owner_uid == p.unix_uid
                            && r.view.role == Role::Term
                            && r.view.state != BindingState::Revoked
                    })
                    .count()
                    >= 64
                {
                    return Err(error(ErrorCode::ResourceLimit, "term_limit"));
                }
                let name = self.issue_name(p.unix_uid, false, reg)?;
                let id = HexBytes(rand::random());
                let view = SessionRecord {
                    name,
                    record_assurance: RecordAssurance::SessionBound,
                    owner_node: p.owner_node.clone(),
                    owner_uid: p.unix_uid,
                    broker_epoch: p.broker_epoch,
                    record_id: id,
                    instance_id: HexBytes(rand::random()),
                    incarnation: HexBytes(rand::random()),
                    role: Role::Term,
                    parent_instance: None,
                    parent_incarnation: None,
                    pane_id: None,
                    pane_generation: None,
                    binding_generation: DecimalU64(1),
                    state: BindingState::Attached,
                    capabilities: vec![
                        Capability::Execute,
                        Capability::Input,
                        Capability::ManageLayout,
                        Capability::ReadContents,
                        Capability::ReadState,
                        Capability::Terminate,
                    ],
                    policy: a.policy,
                    lease_remaining_ms: Some(DecimalU64(LEASE_MS)),
                };
                self.records.insert(
                    id,
                    Record {
                        sequence: self.issued.len(),
                        parent_id: None,
                        grant_id: None,
                        view: view.clone(),
                        key: a.public_key,
                        connection: Some(p.connection_id),
                        deadline: now + LEASE_MS,
                    },
                );
                self.install(id, reg);
                self.connections
                    .get_mut(&p.connection_id)
                    .expect("connection")
                    .binding = Some(id);
                self.notice(id, None);
                Ok(serde_json::json!({"record": view}))
            }
            SessionCommand::Renew(a) => {
                if !self.parent_live(self.owned(p.unix_uid, &a.target)?, now) {
                    return Err(error(ErrorCode::Expired, ""));
                }
                let r = self
                    .records
                    .get_mut(&a.target.record_id)
                    .filter(|r| r.view.owner_uid == p.unix_uid)
                    .ok_or_else(SessionError::forbidden)?;
                if r.view.reference() != a.target {
                    return Err(error(ErrorCode::StaleGeneration, ""));
                }
                if r.view.state != BindingState::Attached {
                    return Err(error(ErrorCode::Expired, ""));
                }
                if r.connection != Some(p.connection_id) {
                    return Err(SessionError::forbidden());
                }
                r.deadline = now + LEASE_MS;
                let view = self.snapshot(&self.records[&a.target.record_id], now);
                Ok(serde_json::json!({"record": view}))
            }
            SessionCommand::SelfRecord(a) => {
                let record = self
                    .records
                    .get(&a.record_id)
                    .filter(|record| record.view.owner_uid == p.unix_uid)
                    .ok_or_else(SessionError::forbidden)?;
                Ok(serde_json::json!({"record": self.snapshot(record, now)}))
            }
            SessionCommand::List => {
                let mut records: Vec<_> = self
                    .records
                    .values()
                    .filter(|r| r.view.owner_uid == p.unix_uid)
                    .map(|r| self.snapshot(r, now))
                    .collect();
                records.sort_by(|a, b| a.name.cmp(&b.name));
                let result =
                    serde_json::json!({"broker_epoch": p.broker_epoch, "records": records});
                if serde_json::to_vec(&result).expect("snapshot").len() > 1024 * 1024 {
                    return Err(error(ErrorCode::ResourceLimit, "snapshot_limit"));
                }
                Ok(result)
            }
        }
    }

    fn grant_create(
        &mut self,
        p: &BrokerPrincipal,
        a: &GrantCreateArgs,
        reg: &mut HashMap<String, ServiceEntry>,
        now: u64,
    ) -> Reply {
        let parent = self.owned(p.unix_uid, &a.parent)?;
        if parent.view.role != Role::Term
            || parent.connection != Some(p.connection_id)
            || self.remaining(parent, now) == 0
        {
            return Err(SessionError::forbidden());
        }
        strict_key(a.public_key)?;
        let parent_view = parent.view.clone();
        let parent_hash = HexBytes(Sha256::digest(parent.key.0).into());
        if self.records.values().any(|r| {
            r.view.owner_uid == p.unix_uid
                && r.key == a.public_key
                && r.view.state != BindingState::Revoked
        }) {
            return Err(error(ErrorCode::Conflict, "key_in_use"));
        }
        if a.pane_generation.0
            <= *self
                .pane_high_water
                .get(&(parent_view.instance_id, a.pane_id.0))
                .unwrap_or(&0)
        {
            return Err(error(ErrorCode::StaleGeneration, ""));
        }
        let pending: Vec<_> = self
            .grants
            .values()
            .filter(|g| g.state == GrantState::Pending)
            .collect();
        if pending.len() >= 1024
            || pending
                .iter()
                .filter(|g| {
                    self.records[&g.record_id].view.parent_instance == Some(parent_view.instance_id)
                })
                .count()
                >= self.pending_grants_per_parent
        {
            return Err(error(ErrorCode::ResourceLimit, "grant_limit"));
        }
        let name = self.issue_name(p.unix_uid, true, reg)?;
        let id = HexBytes(rand::random());
        let mut capabilities = a.capabilities.clone();
        capabilities.sort_by_key(|c| c.as_str());
        let view = SessionRecord {
            name,
            record_assurance: RecordAssurance::Reserved,
            owner_node: p.owner_node.clone(),
            owner_uid: p.unix_uid,
            broker_epoch: p.broker_epoch,
            record_id: id,
            instance_id: HexBytes(rand::random()),
            incarnation: HexBytes(rand::random()),
            role: Role::PaneShell,
            parent_instance: Some(parent_view.instance_id),
            parent_incarnation: Some(parent_view.incarnation),
            pane_id: Some(a.pane_id),
            pane_generation: Some(a.pane_generation),
            binding_generation: DecimalU64(0),
            state: BindingState::Pending,
            capabilities,
            policy: parent_view.policy,
            lease_remaining_ms: None,
        };
        let grant = SessionGrant {
            grant_id: HexBytes(rand::random()),
            record_id: id,
            incarnation: view.incarnation,
            public_key: a.public_key,
            parent_key_hash: parent_hash,
            expires_ms: DecimalU64(now + 30_000),
            state: GrantState::Pending,
        };
        self.pane_high_water
            .insert((parent_view.instance_id, a.pane_id.0), a.pane_generation.0);
        self.records.insert(
            id,
            Record {
                sequence: self.issued.len(),
                parent_id: Some(a.parent.record_id),
                grant_id: Some(grant.grant_id),
                view: view.clone(),
                key: a.public_key,
                connection: None,
                deadline: grant.expires_ms.0,
            },
        );
        self.grants.insert(grant.grant_id, grant.clone());
        self.notice(id, None);
        Ok(serde_json::json!({"grant":grant,"record":view}))
    }

    fn challenge(&mut self, p: &BrokerPrincipal, a: &ChallengeArgs, now: u64) -> Reply {
        let cid = p.connection_id;
        let wake_failed = if let ChallengeArgs::Key(key) = a {
            let existing = self.connections[&cid].interest;
            let full = self
                .connections
                .values()
                .filter(|c| c.principal.unix_uid == p.unix_uid && c.interest.is_some())
                .count()
                >= 256;
            if existing == Some(key.public_key) {
                false
            } else if existing.is_some() || full {
                true
            } else {
                self.connections.get_mut(&cid).expect("connection").interest = Some(key.public_key);
                false
            }
        } else {
            false
        };
        let result = self.challenge_inner(p, a, now);
        if !wake_failed {
            return result;
        }
        // The unsigned extension is added by the response wrapper, including
        // uniform forbidden errors. It never changes lookup or challenge state.
        let wake = serde_json::json!({"error_code":"RESOURCE_LIMIT","message":"wake registration unavailable","details":{"reason":"interest_limit","retry_after_ms":"60000"}});
        match result {
            Ok(mut body) => {
                body["wake_error"] = wake;
                Ok(body)
            }
            Err(mut e) => {
                e.details.insert("wake_error".into(), wake);
                Err(e)
            }
        }
    }

    fn challenge_inner(&mut self, p: &BrokerPrincipal, a: &ChallengeArgs, now: u64) -> Reply {
        let cid = p.connection_id;
        if let Some((selector, proof)) = &self.connections[&cid].challenge {
            if selector == a {
                return Ok(serde_json::to_value(proof).expect("proof"));
            }
            let mut e = error(ErrorCode::Conflict, "challenge_outstanding");
            e.details.insert(
                "retry_after_ms".into(),
                proof
                    .challenge_expires_ms
                    .0
                    .saturating_sub(now)
                    .to_string()
                    .into(),
            );
            return Err(e);
        }
        let (r, purpose, grant) = match a {
            ChallengeArgs::Key(k) => {
                let r = self
                    .records
                    .values()
                    .find(|r| {
                        r.view.owner_uid == p.unix_uid
                            && r.key == k.public_key
                            && r.view.state != BindingState::Revoked
                    })
                    .ok_or_else(SessionError::forbidden)?;
                let purpose = if r.view.state == BindingState::Pending {
                    Purpose::Enrol
                } else {
                    Purpose::Resume
                };
                let grant = (purpose == Purpose::Enrol)
                    .then(|| {
                        self.grants
                            .values()
                            .find(|g| g.record_id == r.view.record_id)
                    })
                    .flatten();
                (r, purpose, grant)
            }
            ChallengeArgs::Record(a) => {
                let r = self
                    .records
                    .get(&a.record_id)
                    .filter(|r| {
                        r.view.owner_uid == p.unix_uid
                            && r.view.incarnation == a.incarnation
                            && r.view.state != BindingState::Revoked
                    })
                    .ok_or_else(SessionError::forbidden)?;
                let grant = if let Some(id) = a.grant_id {
                    Some(
                        self.grants
                            .get(&id)
                            .filter(|g| g.record_id == a.record_id)
                            .ok_or_else(SessionError::forbidden)?,
                    )
                } else {
                    None
                };
                (r, a.purpose, grant)
            }
        };
        if purpose == Purpose::Enrol {
            let g = grant.ok_or_else(SessionError::forbidden)?;
            if g.state == GrantState::Consumed {
                return Err(error(ErrorCode::Conflict, "grant_consumed"));
            }
            if g.state != GrantState::Pending || g.expires_ms.0 <= now {
                return Err(error(ErrorCode::Expired, ""));
            }
        } else if r.view.state == BindingState::Pending {
            return Err(error(ErrorCode::Conflict, "binding_pending"));
        }
        if self
            .connections
            .values()
            .filter(|c| c.principal.unix_uid == p.unix_uid && c.challenge.is_some())
            .count()
            >= 128
        {
            return Err(error(ErrorCode::ResourceLimit, "challenge_limit"));
        }
        let v = &r.view;
        let generation = if purpose == Purpose::Enrol {
            1
        } else {
            v.binding_generation
                .0
                .checked_add(1)
                .ok_or_else(|| error(ErrorCode::ResourceLimit, "generation_limit"))?
        };
        let proof = ProofTranscript {
            purpose,
            broker_epoch: p.broker_epoch,
            connection_id: cid,
            challenge_id: HexBytes(rand::random()),
            nonce: HexBytes(rand::random()),
            grant_id: grant.map(|g| g.grant_id),
            record_id: v.record_id,
            instance_id: v.instance_id,
            incarnation: v.incarnation,
            unix_uid: p.unix_uid,
            parent_instance: v.parent_instance,
            parent_incarnation: v.parent_incarnation,
            parent_key_hash: self
                .parent(r)
                .map(|parent| HexBytes(Sha256::digest(parent.key.0).into())),
            pane_id: v.pane_id,
            pane_generation: v.pane_generation,
            role: v.role,
            public_key_hash: HexBytes(Sha256::digest(r.key.0).into()),
            capabilities_hash: HexBytes(
                Sha256::digest(encode_capabilities(&v.capabilities).expect("stored capabilities"))
                    .into(),
            ),
            binding_generation: DecimalU64(generation),
            grant_expires_ms: grant.map(|g| g.expires_ms),
            challenge_expires_ms: DecimalU64((now + 5000).min(r.deadline)),
        };
        self.connections
            .get_mut(&cid)
            .expect("connection")
            .challenge = Some((a.clone(), proof.clone()));
        Ok(serde_json::to_value(proof).expect("proof"))
    }

    pub(super) fn consume_malformed(&mut self, cid: Id) {
        self.deadline_wake.notify_one();
        if let Some(c) = self.connections.get_mut(&cid)
            && let Some((_, p)) = c.challenge.take()
        {
            Self::remember_consumed(c, p.challenge_id);
        }
    }

    fn remember_consumed(c: &mut Connection, id: Id) {
        if c.consumed.len() >= 1024
            && let Some(old) = c.consumed.iter().next().copied()
        {
            c.consumed.remove(&old);
        }
        c.consumed.insert(id);
    }

    fn prove(
        &mut self,
        p: &BrokerPrincipal,
        a: &ProveArgs,
        reg: &mut HashMap<String, ServiceEntry>,
        now: u64,
    ) -> Reply {
        let c = self
            .connections
            .get_mut(&p.connection_id)
            .expect("connection");
        let outstanding = c.challenge.take();
        if let Some((_, proof)) = &outstanding {
            Self::remember_consumed(c, proof.challenge_id);
        }
        if c.consumed.contains(&a.challenge_id)
            && outstanding
                .as_ref()
                .is_none_or(|(_, proof)| proof.challenge_id != a.challenge_id)
        {
            return Err(error(ErrorCode::Conflict, "challenge_consumed"));
        }
        let (_, proof) = outstanding
            .filter(|(_, proof)| proof.challenge_id == a.challenge_id)
            .ok_or_else(SessionError::forbidden)?;
        if proof.challenge_expires_ms.0 <= now {
            return Err(error(ErrorCode::Expired, ""));
        }
        let r = self
            .records
            .get(&proof.record_id)
            .filter(|r| r.view.owner_uid == p.unix_uid && r.view.incarnation == proof.incarnation)
            .ok_or_else(SessionError::forbidden)?;
        if r.view.state == BindingState::Revoked || !self.parent_live(r, now) {
            return Err(error(ErrorCode::Expired, ""));
        }
        if self.connections[&p.connection_id]
            .binding
            .is_some_and(|id| id != proof.record_id)
        {
            return Err(error(ErrorCode::Conflict, "already_bound"));
        }
        if r.view.binding_generation.0.checked_add(1) != Some(proof.binding_generation.0) {
            return Err(error(ErrorCode::StaleGeneration, ""));
        }
        verify(
            r.key,
            a.signature,
            &encode_proof(&proof).map_err(|_| SessionError::forbidden())?,
        )?;
        if let Some(id) = proof.grant_id {
            let g = self
                .grants
                .get_mut(&id)
                .ok_or_else(SessionError::forbidden)?;
            if g.state != GrantState::Pending || g.expires_ms.0 <= now {
                return Err(error(ErrorCode::Conflict, "grant_consumed"));
            }
            g.state = GrantState::Consumed;
        }
        let r = self.records.get_mut(&proof.record_id).expect("record");
        let closing = r.connection;
        if let Some(old) = r.connection
            && old != p.connection_id
            && let Some(c) = self.connections.get(&old)
        {
            c.close.notify_one();
        }
        r.connection = Some(p.connection_id);
        r.view.state = BindingState::Attached;
        r.view.record_assurance = RecordAssurance::SessionBound;
        r.view.binding_generation = proof.binding_generation;
        r.deadline = now + LEASE_MS;
        self.install(proof.record_id, reg);
        self.connections
            .get_mut(&p.connection_id)
            .expect("connection")
            .binding = Some(proof.record_id);
        self.notice(proof.record_id, closing);
        for child in self.children(proof.record_id) {
            if self.records[&child].view.state != BindingState::Revoked {
                self.notice(child, None);
            }
        }
        Ok(serde_json::json!({"record":self.snapshot(&self.records[&proof.record_id],now)}))
    }
}

#[cfg(test)]
pub(super) mod queue_tests {
    use super::*;
    fn allocated() -> (Sessions, HashMap<String, ServiceEntry>, BrokerPrincipal, Id) {
        allocated_at(1000)
    }

    pub(crate) fn allocated_at(
        now: u64,
    ) -> (Sessions, HashMap<String, ServiceEntry>, BrokerPrincipal, Id) {
        use ed25519_dalek::{Signer, SigningKey};
        let mut s = Sessions::default();
        let mut reg = HashMap::new();
        let p = BrokerPrincipal {
            version: PrincipalVersion::V1,
            assurance: Assurance::LocalUnix,
            owner_node: "alpha".into(),
            unix_uid: 123,
            unix_gid: 123,
            peer_pid: 1,
            broker_epoch: HexBytes([1; 16]),
            connection_id: HexBytes([2; 16]),
            session: None,
        };
        let (tx, _rx) = mpsc::channel(1);
        s.connect(
            &p,
            &tx,
            Arc::new(tokio::sync::Notify::new()),
            Default::default(),
        );
        let key = SigningKey::from_bytes(&rand::random());
        let public_key = HexBytes(key.verifying_key().to_bytes());
        let signature = HexBytes(
            key.sign(&encode_allocate(
                p.broker_epoch,
                p.connection_id,
                public_key,
                Policy::Restricted,
            ))
            .to_bytes(),
        );
        let command = SessionCommand::Allocate(AllocateArgs {
            public_key,
            signature,
            policy: Policy::Restricted,
        });
        s.dispatch(&p, &command, &mut reg, now).unwrap();
        let id = s.attached(p.connection_id).unwrap();
        (s, reg, p, id)
    }

    #[test]
    fn targeted_owner_read_survives_oversized_history_and_hides_foreign_ids() {
        let (mut s, mut reg, p, id) = allocated();
        // Retained terminal history, each entry a normal bounded record shape.
        for index in 100u128..2100 {
            let original = &s.records[&id];
            let mut view = original.view.clone();
            let key = original.key;
            view.record_id = HexBytes(index.to_be_bytes());
            view.name = format!("retained-{index}");
            view.state = BindingState::Revoked;
            s.records.insert(
                view.record_id,
                Record {
                    sequence: index as usize,
                    parent_id: None,
                    grant_id: None,
                    view,
                    key,
                    connection: None,
                    deadline: 0,
                },
            );
        }
        assert_eq!(
            s.dispatch(&p, &SessionCommand::List, &mut reg, 1001)
                .unwrap_err()
                .details["reason"],
            "snapshot_limit"
        );
        let command = SessionCommand::SelfRecord(SelfArgs { record_id: id });
        let result = s.dispatch(&p, &command, &mut reg, 1001).unwrap();
        assert_eq!(
            result["record"]["record_id"],
            serde_json::to_value(id).unwrap()
        );
        assert!(serde_json::to_vec(&result).unwrap().len() < 4096);
        let mut foreign = p.clone();
        foreign.unix_uid += 1;
        assert_eq!(
            s.dispatch(&foreign, &command, &mut reg, 1001).unwrap_err(),
            SessionError::forbidden()
        );
        assert_eq!(
            s.dispatch(
                &p,
                &SessionCommand::SelfRecord(SelfArgs {
                    record_id: HexBytes([255; 16])
                }),
                &mut reg,
                1001
            )
            .unwrap_err(),
            SessionError::forbidden()
        );
    }

    #[test]
    fn restricted_revoke_does_not_disclose_terminal_state() {
        let (mut s, mut reg, p, id) = allocated();
        let mut stranger = p.clone();
        stranger.connection_id = HexBytes([9; 16]);
        let command = SessionCommand::Revoke(TargetArgs {
            target: s.records[&id].view.reference(),
        });
        let live_error = s.dispatch(&stranger, &command, &mut reg, 1001).unwrap_err();
        s.revoke(id, &mut reg);
        let terminal_error = s.dispatch(&stranger, &command, &mut reg, 1002).unwrap_err();
        assert_eq!(live_error, SessionError::forbidden());
        assert_eq!(terminal_error, live_error);
    }

    #[tokio::test]
    async fn one_scheduler_rearms_for_an_earlier_deadline_without_traffic() {
        let now = now_ms().unwrap();
        let (s, reg, p, id) = allocated_at(now);
        let close = s.connections[&p.connection_id].close.clone();
        let sessions = Arc::new(tokio::sync::Mutex::new(s));
        let registry = Arc::new(RwLock::new(reg));
        let _scheduler = spawn_maintenance(
            registry.clone(),
            sessions.clone(),
            deadline_timer().unwrap(),
        );
        // Re-arm while the scheduler may already be sleeping on the old lease.
        let deadline = now_ms().unwrap() + 30;
        {
            let mut s = sessions.lock().await;
            s.records.get_mut(&id).unwrap().deadline = deadline;
            s.deadline_wake.notify_one();
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), close.notified())
            .await
            .unwrap();
        let reg = registry.read().await;
        let s = sessions.lock().await;
        assert!(reg.is_empty());
        assert_eq!(s.records[&id].view.state, BindingState::Suspended);
        assert_eq!(s.next_deadline(), Some(deadline + RESUME_MS));
    }

    #[test]
    fn routing_refuses_expired_target_before_scheduler_removes_route() {
        let (mut s, reg, _, id) = allocated_at(now_ms().unwrap().saturating_sub(LEASE_MS));
        let name = s.records[&id].view.name.clone();
        assert!(reg.contains_key(&name));
        assert_eq!(
            s.validate_route(&name).unwrap_err().error_code,
            ErrorCode::Expired
        );
    }

    #[test]
    fn clock_failure_revokes_and_cannot_mint_new_authority() {
        let (mut s, mut reg, p, id) = allocated();
        s.fail_clock(&mut reg);
        assert!(reg.is_empty());
        assert_eq!(s.records[&id].view.state, BindingState::Revoked);
        assert_eq!(
            s.maintain_now(&mut reg).unwrap_err().error_code,
            ErrorCode::Unavailable
        );
        let (tx, _rx) = mpsc::channel(1);
        assert_eq!(
            s.delivery_now(&p, &tx).unwrap_err().error_code,
            ErrorCode::Unavailable
        );
    }

    #[tokio::test]
    async fn expired_publisher_errors_even_without_subscribers() {
        for subscribers in [0, 1] {
            let now = now_ms().unwrap();
            let (s, _, p, _) = allocated_at(now.saturating_sub(LEASE_MS));
            let p = s.principal(p.connection_id, now).unwrap();
            let broker = subscription::SubscriptionBroker::new();
            broker.set_native_sessions(Arc::new(tokio::sync::Mutex::new(s)));
            let (tx, mut rx) = mpsc::channel(8);
            if subscribers != 0 {
                broker
                    .subscribe_topic("expired.test", "recipient", tx.clone())
                    .await;
            }
            let wire = BusMessage::new()
                .with_header("type", "event")
                .with_header("command", "snapshot")
                .to_wire();
            let error = broker
                .publish_with_principal(
                    "expired.test",
                    &wire,
                    "publisher",
                    tx,
                    subscription::BrokerOrigin::Local,
                    true,
                    Some(&p),
                )
                .await
                .unwrap_err();
            assert_eq!(
                error.session_error().unwrap().error_code,
                ErrorCode::Expired
            );
            assert_eq!(error.rc(), 10);
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn fanout_refuses_only_the_recipient_at_dependency_capacity() {
        let (mut s, _, p, id) = allocated();
        let now = now_ms().unwrap();
        s.records.get_mut(&id).unwrap().deadline = now + LEASE_MS;
        let p = s.principal(p.connection_id, now).unwrap();
        let broker = subscription::SubscriptionBroker::new();
        let mut receivers = Vec::new();
        for index in 0..3 {
            let (tx, rx) = mpsc::channel(8);
            let cid = HexBytes([index + 10; 16]);
            s.open_outbox(cid, &tx, Default::default());
            if index == 1 {
                let reference = RecordRef {
                    record_id: HexBytes([99; 16]),
                    incarnation: HexBytes([99; 16]),
                    binding_generation: DecimalU64(1),
                };
                s.outboxes.get_mut(&cid).unwrap().dependencies =
                    vec![(reference, now + LEASE_MS); 256];
            }
            broker
                .subscribe_topic_verified(
                    "session.test",
                    &format!("recipient{index}"),
                    tx,
                    None,
                    true,
                )
                .await;
            receivers.push(rx);
        }
        broker.set_native_sessions(Arc::new(tokio::sync::Mutex::new(s)));
        let (publisher, _rx) = mpsc::channel(8);
        let wire = BusMessage::new()
            .with_header("type", "event")
            .with_header("command", "snapshot")
            .to_wire();
        let outcome = broker
            .publish_with_principal(
                "session.test",
                &wire,
                "publisher",
                publisher,
                subscription::BrokerOrigin::Local,
                false,
                Some(&p),
            )
            .await
            .unwrap();
        assert_eq!(
            (outcome.delivered, outcome.refused, outcome.dropped),
            (2, 1, 0)
        );
        assert_eq!(outcome.body()["partial"], true);
        assert!(receivers[0].try_recv().is_ok());
        assert!(receivers[1].try_recv().is_err());
        assert!(receivers[2].try_recv().is_ok());
    }

    #[test]
    fn dispatch_expires_without_caller_maintenance() {
        let (mut s, mut reg, p, id) = allocated();
        s.dispatch(&p, &SessionCommand::Hello, &mut reg, 50_000)
            .unwrap();
        assert_eq!(s.records[&id].view.state, BindingState::Revoked);
        assert!(reg.is_empty());
    }

    #[test]
    fn replacement_notifies_closing_channel_and_preserves_its_scope() {
        use ed25519_dalek::{Signer, SigningKey};
        let (mut s, mut reg, p, id) = allocated();
        let key = SigningKey::from_bytes(&rand::random());
        let public_key = HexBytes(key.verifying_key().to_bytes());
        s.records.get_mut(&id).unwrap().key = public_key;
        let old_tx = s.connections[&p.connection_id].tx.clone();
        s.open_outbox(p.connection_id, &old_tx, Default::default());
        let mut successor = p.clone();
        successor.connection_id = HexBytes([3; 16]);
        let (tx, _rx) = mpsc::channel(1);
        s.connect(&successor, &tx, Default::default(), Default::default());
        let selector = ChallengeArgs::Key(KeyChallenge {
            public_key,
            purpose: Purpose::Enrol,
        });
        let proof: ProofTranscript =
            serde_json::from_value(s.challenge(&successor, &selector, 1001).unwrap()).unwrap();
        s.prove(
            &successor,
            &ProveArgs {
                challenge_id: proof.challenge_id,
                signature: HexBytes(key.sign(&encode_proof(&proof).unwrap()).to_bytes()),
            },
            &mut reg,
            1002,
        )
        .unwrap();
        let (_, wire) = s.next_notice(p.connection_id, p.broker_epoch).unwrap();
        let notice = bus::parse(&wire).unwrap();
        let body: serde_json::Value = serde_json::from_str(&notice.body).unwrap();
        assert_eq!(body["target"]["binding_generation"], "2");
        assert_eq!(body["state"], "attached");
        assert_eq!(s.attached(p.connection_id), None);
        assert_eq!(s.attached(successor.connection_id), Some(id));

        // The old read loop has not observed its close notification yet.
        let other_key = SigningKey::from_bytes(&rand::random());
        let other_public = HexBytes(other_key.verifying_key().to_bytes());
        let allocation = SessionCommand::Allocate(AllocateArgs {
            public_key: other_public,
            signature: HexBytes(
                other_key
                    .sign(&encode_allocate(
                        p.broker_epoch,
                        p.connection_id,
                        other_public,
                        Policy::Restricted,
                    ))
                    .to_bytes(),
            ),
            policy: Policy::Restricted,
        });
        assert_eq!(
            s.dispatch(&p, &allocation, &mut reg, 1003)
                .unwrap_err()
                .details["reason"],
            "already_bound"
        );
        // An otherwise valid proof must not rename the retiring channel either.
        let (other, _, _, other_id) = allocated();
        let mut other_record = other.records.into_values().next().unwrap();
        other_record.key = other_public;
        other_record.connection = None;
        other_record.view.state = BindingState::Suspended;
        s.records.insert(other_id, other_record);
        let proof: ProofTranscript = serde_json::from_value(
            s.challenge(
                &p,
                &ChallengeArgs::Key(KeyChallenge {
                    public_key: other_public,
                    purpose: Purpose::Enrol,
                }),
                1003,
            )
            .unwrap(),
        )
        .unwrap();
        let refused = s
            .prove(
                &p,
                &ProveArgs {
                    challenge_id: proof.challenge_id,
                    signature: HexBytes(other_key.sign(&encode_proof(&proof).unwrap()).to_bytes()),
                },
                &mut reg,
                1004,
            )
            .unwrap_err();
        assert_eq!(refused.details["reason"], "already_bound");
    }

    #[test]
    fn delivery_releases_expired_dependencies_without_a_maintenance_tick() {
        let (mut s, _, p, id) = allocated();
        let recipient = HexBytes([3; 16]);
        let (tx, _rx) = mpsc::channel(1);
        s.open_outbox(recipient, &tx, Default::default());
        let reference = s.records[&id].view.reference();
        for generation in 2..=257 {
            let mut target = reference.clone();
            target.binding_generation = DecimalU64(generation);
            s.outboxes
                .get_mut(&recipient)
                .unwrap()
                .dependencies
                .push((target, 1001));
        }
        assert_eq!(
            s.delivery(&p, &tx, 1000).unwrap_err().details["reason"],
            "recipient_dependency_limit"
        );
        s.delivery(&p, &tx, 1001).unwrap();
        assert_eq!(
            s.outboxes[&recipient].dependencies,
            vec![(reference, 16_000)]
        );
    }

    #[test]
    fn lease_check_refreshes_recipient_notice_dependency() {
        let (mut s, mut reg, p, id) = allocated();
        let mut recipient = p.clone();
        recipient.connection_id = HexBytes([3; 16]);
        let (tx, _rx) = mpsc::channel(1);
        s.connect(
            &recipient,
            &tx,
            Arc::new(tokio::sync::Notify::new()),
            Default::default(),
        );
        s.open_outbox(
            recipient.connection_id,
            &tx,
            Arc::new(tokio::sync::Notify::new()),
        );
        s.delivery(&p, &tx, 1000).unwrap();
        let target = s.records[&id].view.reference();
        s.dispatch(
            &p,
            &SessionCommand::Renew(TargetArgs {
                target: target.clone(),
            }),
            &mut reg,
            6000,
        )
        .unwrap();
        let checked = s
            .dispatch(
                &recipient,
                &SessionCommand::LeaseCheck(TargetArgs { target }),
                &mut reg,
                7000,
            )
            .unwrap();
        assert_eq!(checked["lease_remaining_ms"], "14000");
        assert_eq!(
            s.outboxes[&recipient.connection_id].dependencies[0].1,
            21_000
        );
        s.maintain(&mut reg, 16_000);
        s.suspend(id, &mut reg, 16_000);
        assert!(
            s.next_notice(recipient.connection_id, p.broker_epoch)
                .is_some()
        );
    }

    #[test]
    fn lease_boundary_and_delayed_sweep_do_not_extend_resumption() {
        let (mut s, mut reg, p, id) = allocated();
        let reference = s.records[&id].view.reference();
        s.maintain(&mut reg, 15_999);
        assert_eq!(s.records[&id].view.state, BindingState::Attached);
        s.maintain(&mut reg, 16_000);
        assert_eq!(s.records[&id].view.state, BindingState::Suspended);
        assert_eq!(s.records[&id].deadline, 46_000);
        assert!(
            s.dispatch(
                &p,
                &SessionCommand::Renew(TargetArgs { target: reference }),
                &mut reg,
                16_000
            )
            .is_err()
        );
        s.suspend(id, &mut reg, 20_000);
        assert_eq!(s.records[&id].deadline, 46_000);
        s.maintain(&mut reg, 46_000);
        assert_eq!(s.records[&id].view.state, BindingState::Revoked);
        let (mut s, mut reg, _, id) = allocated();
        s.maintain(&mut reg, 50_000);
        assert_eq!(s.records[&id].view.state, BindingState::Revoked);
    }

    #[test]
    fn retained_expiry_keeps_unknown_outcome_high_water() {
        // execute() samples real BOOTTIME, unlike the synthetic-clock tests.
        // Start with a live owned lease so this exercises result retention,
        // rather than an expired record's uniform revoke refusal.
        let (mut s, mut reg, p, id) = allocated_at(now_ms().unwrap());
        let args = TargetArgs {
            target: s.records[&id].view.reference(),
        };
        let message = BusMessage::new().with_header("id", "7");
        let request = BootstrapRequest {
            message,
            command: SessionCommand::Revoke(args),
        };
        let first = s.execute(&p, &request, &mut reg);
        assert_eq!(first.as_ref().unwrap()["revoked"], true);
        assert_eq!(s.execute(&p, &request, &mut reg), first);
        for result in &mut s.results {
            result.expires = 0;
        }
        assert_eq!(
            s.execute(&p, &request, &mut reg).unwrap_err().details["reason"],
            "unknown_outcome"
        );
    }
    #[test]
    fn notice_overflow_coalesces_and_global_shedding_marks_victim() {
        let mut s = Sessions::default();
        let (tx, _rx) = mpsc::channel(1);
        let epoch = HexBytes([1; 16]);
        for n in 0..17 {
            let id = HexBytes([n; 16]);
            s.open_outbox(id, &tx, Arc::new(tokio::sync::Notify::new()));
        }
        let first = HexBytes([0; 16]);
        for _ in 0..257 {
            s.queue_notice(first, "notice".into());
        }
        assert_eq!(s.outboxes[&first].notices.len(), 256);
        assert!(s.next_notice(first, epoch).unwrap().0);
        assert!(!s.next_notice(first, epoch).unwrap().0);
        for n in 1..17 {
            for _ in 0..256 {
                s.queue_notice(HexBytes([n; 16]), "notice".into());
            }
        }
        assert_eq!(
            s.outboxes.values().map(|o| o.notices.len()).sum::<usize>(),
            4096
        );
        assert!(s.outboxes.values().any(|o| o.gap));
        s.restore_gap(first);
        assert!(s.next_notice(first, epoch).unwrap().0);
    }
}
