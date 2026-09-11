//! Native binding state. Registry lock precedes this lock everywhere; all
//! authority transitions and route installation happen in that critical section.
use super::*;
use cosmix_bus::native_session::*;
use ed25519_dalek::{Signature, VerifyingKey};
use std::collections::{HashSet, VecDeque};

const LEASE_MS: u64 = 15_000;
const RESUME_MS: u64 = 30_000;
const MAX_ISSUED: usize = 65_536;
type Id = HexBytes<16>;
type Reply = Result<serde_json::Value, SessionError>;

pub(super) fn now_ms() -> u64 {
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
    let key = VerifyingKey::from_bytes(&key.0).map_err(|_| SessionError::forbidden())?;
    if key.is_weak() {
        return Err(SessionError::forbidden());
    }
    key.verify_strict(bytes, &Signature::from_bytes(&signature.0))
        .map_err(|_| SessionError::forbidden())
}

struct Record {
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
}

#[derive(Default)]
pub(super) struct Sessions {
    records: HashMap<Id, Record>,
    connections: HashMap<Id, Connection>,
    issued: HashSet<String>,
    results: VecDeque<Cached>,
}

impl Sessions {
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
            },
        );
    }

    fn snapshot(&self, record: &Record, now: u64) -> SessionRecord {
        let mut view = record.view.clone();
        view.lease_remaining_ms = (view.state == BindingState::Attached)
            .then(|| DecimalU64(record.deadline.saturating_sub(now)));
        view
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
        let r = self.records.get_mut(&id).expect("record");
        if r.view.state != BindingState::Attached {
            return;
        }
        if let Some(c) = r.connection.take().and_then(|id| self.connections.get(&id)) {
            if reg.get(&r.view.name).is_some_and(|e| e.same_channel(&c.tx)) {
                reg.remove(&r.view.name);
            }
            c.close.notify_one();
        }
        r.view.state = BindingState::Suspended;
        r.deadline = now.saturating_add(RESUME_MS);
    }

    pub(super) fn maintain(&mut self, reg: &mut HashMap<String, ServiceEntry>, now: u64) {
        let expired: Vec<_> = self
            .records
            .iter()
            .filter(|(_, r)| {
                r.deadline <= now
                    && matches!(
                        r.view.state,
                        BindingState::Attached | BindingState::Suspended
                    )
            })
            .map(|(id, r)| (*id, r.view.state))
            .collect();
        for (id, state) in expired {
            if state == BindingState::Attached {
                self.suspend(id, reg, now);
            } else {
                self.records.get_mut(&id).expect("record").view.state = BindingState::Revoked;
            }
        }
        self.results.retain(|r| r.expires > now);
    }

    pub(super) fn disconnect(&mut self, id: Id, reg: &mut HashMap<String, ServiceEntry>) {
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

    pub(super) fn principal(&self, connection: Id, now: u64) -> Option<BrokerPrincipal> {
        let mut p = self.connections.get(&connection)?.principal.clone();
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
                lease_remaining_ms: DecimalU64(r.deadline.saturating_sub(now)),
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
                        view: view.clone(),
                        key: a.public_key,
                        connection: Some(p.connection_id),
                        deadline: now + LEASE_MS,
                    },
                );
                self.install(id, reg);
                Ok(serde_json::json!({"record": view}))
            }
            SessionCommand::Renew(a) => {
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
            _ => Err(error(ErrorCode::Unsupported, "")),
        }
    }
}
