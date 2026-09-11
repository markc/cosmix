//! Native binding state. Registry lock precedes this lock everywhere; all
//! authority transitions and route installation happen in that critical section.
use super::*;
use cosmix_bus::native_session::*;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};

const LEASE_MS: u64 = 15_000;
const RESUME_MS: u64 = 30_000;
const MAX_ISSUED: usize = 65_536;
type Id = HexBytes<16>;
type Reply = Result<serde_json::Value, SessionError>;

#[cfg(test)]
mod queue_tests {
    use super::*;
    fn allocated() -> (Sessions, HashMap<String, ServiceEntry>, BrokerPrincipal, Id) {
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
        s.dispatch(&p, &command, &mut reg, 1000).unwrap();
        let id = s.attached(p.connection_id).unwrap();
        (s, reg, p, id)
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
        let (mut s, mut reg, p, id) = allocated();
        let args = TargetArgs {
            target: s.records[&id].view.reference(),
        };
        let message = BusMessage::new().with_header("id", "7");
        let request = BootstrapRequest {
            message,
            command: SessionCommand::Revoke(args),
        };
        let first = s.execute(&p, &request, &mut reg);
        assert!(first.is_ok());
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

pub(crate) fn now_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid writable timespec. Failure must not mint authority.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) },
        0
    );
    (ts.tv_sec as u64).saturating_mul(1000) + ts.tv_nsec as u64 / 1_000_000
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

#[derive(Default)]
pub(crate) struct Sessions {
    records: HashMap<Id, Record>,
    connections: HashMap<Id, Connection>,
    issued: HashSet<String>,
    results: VecDeque<Cached>,
    grants: HashMap<Id, SessionGrant>,
    pane_high_water: HashMap<(Id, u64), u64>,
    outboxes: HashMap<Id, Outbox>,
}

impl Sessions {
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

    pub(crate) fn delivery(
        &mut self,
        p: &BrokerPrincipal,
        target: &mpsc::Sender<String>,
        now: u64,
    ) -> Result<Option<BrokerPrincipal>, SessionError> {
        let principal = self.principal(p.connection_id, now);
        let Some(id) = self.attached(p.connection_id) else {
            // A stale cached bound principal cannot fall back to ambient authority.
            if p.session.is_some() {
                return Err(error(ErrorCode::Expired, ""));
            }
            return Ok(principal);
        };
        let r = &self.records[&id];
        let remaining = self.remaining(r, now);
        if remaining == 0 {
            return Err(error(ErrorCode::Expired, ""));
        }
        let reference = r.view.reference();
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

    pub(super) fn maintain(&mut self, reg: &mut HashMap<String, ServiceEntry>, now: u64) {
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
        self.maintain(reg, now_ms());
        if let Some(record) = self.attached(id) {
            self.suspend(record, reg, now_ms());
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
        let now = now_ms();
        self.maintain(reg, now);
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
            while self.results.len() >= 8192
                || self.results.iter().map(|r| r.bytes).sum::<usize>() + bytes > 16 * 1024 * 1024
            {
                self.results.pop_front();
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
                if !owner && r.view.state != BindingState::Revoked {
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
                if self.attached(p.connection_id).is_some() {
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
                >= 32
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
                (proof.challenge_expires_ms.0 - now).to_string().into(),
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
        if self
            .attached(p.connection_id)
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
        self.notice(proof.record_id, None);
        for child in self.children(proof.record_id) {
            if self.records[&child].view.state != BindingState::Revoked {
                self.notice(child, None);
            }
        }
        Ok(serde_json::json!({"record":self.snapshot(&self.records[&proof.record_id],now)}))
    }
}
