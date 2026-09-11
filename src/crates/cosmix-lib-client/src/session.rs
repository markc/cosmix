//! Typed BUS-015 commands. No retries or transport downgrades are performed.
//! After an uncertain mutation, reconcile by key before submitting new work.
use crate::VerifiedConnection;
use cosmix_bus::native_session::*;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Debug)]
pub enum SessionFailure {
    Transport(anyhow::Error),
    InvalidResponse,
    ScopeMismatch,
    LeaseExpired,
    Refused {
        error: SessionError,
        wake_error: Option<SessionError>,
    },
}
impl std::fmt::Display for SessionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Transport(_) => "session transport failed; outcome may be unknown",
            Self::InvalidResponse => "invalid session response",
            Self::ScopeMismatch => "challenge does not match expected scope",
            Self::LeaseExpired => "lease check elapsed before receipt",
            Self::Refused { .. } => "broker refused session request",
        })
    }
}
impl std::error::Error for SessionFailure {}
pub type SessionResult<T> = Result<T, SessionFailure>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub broker_epoch: HexBytes<16>,
    pub connection_id: HexBytes<16>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordResult {
    pub record: SessionRecord,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantResult {
    pub grant: SessionGrant,
    pub record: SessionRecord,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResult {
    pub broker_epoch: HexBytes<16>,
    pub records: Vec<SessionRecord>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeResult {
    pub revoked: bool,
}
#[derive(Deserialize)]
struct LeaseResult {
    lease_remaining_ms: DecimalU64,
}

/// Conservative local CLOCK_BOOTTIME deadline for one checked reference.
/// Validate against hello from the current connection; discard on lifecycle gaps.
#[derive(Debug, Clone)]
pub struct Deadline {
    target: RecordRef,
    broker_epoch: HexBytes<16>,
    connection_id: HexBytes<16>,
    expires_ms: u64,
}

impl Deadline {
    pub fn target(&self) -> &RecordRef {
        &self.target
    }

    /// Supply context from the connection that produced this deadline, never a
    /// hello retained across a reconnect. `session_context` is that context:
    /// it is scoped to one connection and discarded with it.
    pub fn is_live(&self, current: &Hello) -> SessionResult<bool> {
        if self.broker_epoch != current.broker_epoch || self.connection_id != current.connection_id
        {
            return Ok(false);
        }
        Ok(boottime_ms()? < self.expires_ms)
    }
}

/// Suspend-inclusive milliseconds on the native session clock.
pub fn boottime_ms() -> SessionResult<u64> {
    #[cfg(target_os = "linux")]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: ts is a valid writable timespec.
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
            return Err(SessionFailure::Transport(
                std::io::Error::last_os_error().into(),
            ));
        }
        Ok((ts.tv_sec as u64)
            .saturating_mul(1000)
            .saturating_add(ts.tv_nsec as u64 / 1_000_000))
    }
    #[cfg(not(target_os = "linux"))]
    Err(SessionFailure::Transport(anyhow::anyhow!(
        "CLOCK_BOOTTIME is unavailable"
    )))
}

#[derive(Debug, Clone)]
pub struct ChallengeResult {
    pub transcript: ProofTranscript,
    /// Unsigned quota diagnostic. Surface this instead of waiting indefinitely.
    pub wake_error: Option<SessionError>,
}

/// Retained expectations from the application's authenticated launch descriptor.
/// Hashes use BUS-016 SHA-256, never JSON hashing. Random parent IDs deliberately
/// do not anchor continuity: the allocation-proven parent key hash does.
#[derive(Debug, Clone)]
pub struct ExpectedScope {
    /// From hello on the current verified connection, never the challenge.
    pub broker_epoch: HexBytes<16>,
    /// Enrol for a pending child, resume for an existing attachment.
    pub purpose: Purpose,
    pub unix_uid: u32,
    pub parent_key_hash: Option<HexBytes<32>>,
    pub pane_id: Option<DecimalU64>,
    /// High-water for this (parent_instance, pane_id); reset for a new parent instance.
    pub pane_high_water: Option<DecimalU64>,
    pub role: Role,
    pub public_key_hash: HexBytes<32>,
    pub capabilities_hash: HexBytes<32>,
}

impl ChallengeResult {
    /// The caller supplies independently retained scope, not a copy of this
    /// challenge. Retain pane-generation high-water within each parent instance;
    /// parent random IDs may change during broker recovery.
    pub fn sign(&self, key: &SigningKey, expected: &ExpectedScope) -> SessionResult<ProveArgs> {
        let p = &self.transcript;
        if p.broker_epoch != expected.broker_epoch
            || p.purpose != expected.purpose
            || p.unix_uid != expected.unix_uid
            || p.parent_key_hash != expected.parent_key_hash
            || p.pane_id != expected.pane_id
            || expected.pane_high_water.is_some_and(|high| {
                p.pane_generation
                    .is_none_or(|generation| generation.0 < high.0)
            })
            || p.role != expected.role
            || p.public_key_hash != expected.public_key_hash
            || p.capabilities_hash != expected.capabilities_hash
        {
            return Err(SessionFailure::ScopeMismatch);
        }
        let bytes = encode_proof(p).map_err(|_| SessionFailure::InvalidResponse)?;
        Ok(ProveArgs {
            challenge_id: p.challenge_id,
            signature: HexBytes(key.sign(&bytes).to_bytes()),
        })
    }
}

impl VerifiedConnection {
    async fn session_rpc<T: DeserializeOwned>(
        &self,
        suffix: &str,
        args: impl Serialize,
    ) -> SessionResult<T> {
        let _serial = self.session_lock.lock().await;
        let body = serde_json::to_string(&args).map_err(|_| SessionFailure::InvalidResponse)?;
        let response = self
            .client()
            .session_request(&format!("noded.session.{suffix}"), body)
            .await
            .map_err(SessionFailure::Transport)?;
        if response.get("rc") == Some("0") {
            return serde_json::from_str(&response.body)
                .map_err(|_| SessionFailure::InvalidResponse);
        }
        let mut body: serde_json::Value =
            serde_json::from_str(&response.body).map_err(|_| SessionFailure::InvalidResponse)?;
        let wake_error = body
            .as_object_mut()
            .and_then(|o| o.remove("wake_error"))
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| SessionFailure::InvalidResponse)?;
        let error = serde_json::from_value(body).map_err(|_| SessionFailure::InvalidResponse)?;
        Err(SessionFailure::Refused { error, wake_error })
    }
    pub async fn session_hello(&self) -> SessionResult<Hello> {
        self.session_rpc("hello", serde_json::json!({})).await
    }
    /// The broker epoch and connection id hello reports are fixed for the life
    /// of a verified connection, and this handle never reconnects
    /// transparently, so one hello answers every later caller. Callers that
    /// need connection context on a hot path (lease checks, admission) must
    /// use this rather than `session_hello`: each RPC takes `session_lock`, so
    /// a per-check hello serialises ahead of the resident's own renew.
    pub async fn session_context(&self) -> SessionResult<Hello> {
        match self
            .session_context
            .get_or_try_init(|| self.session_hello())
            .await
        {
            Ok(hello) => Ok(hello.clone()),
            Err(error) => Err(error),
        }
    }
    pub async fn session_allocate(
        &self,
        key: &SigningKey,
        policy: Policy,
    ) -> SessionResult<RecordResult> {
        let hello = self.session_context().await?;
        let public_key = HexBytes(key.verifying_key().to_bytes());
        let signature = HexBytes(
            key.sign(&encode_allocate(
                hello.broker_epoch,
                hello.connection_id,
                public_key,
                policy,
            ))
            .to_bytes(),
        );
        self.session_rpc(
            "allocate",
            AllocateArgs {
                public_key,
                signature,
                policy,
            },
        )
        .await
    }
    pub async fn session_grant_create(&self, args: &GrantCreateArgs) -> SessionResult<GrantResult> {
        self.session_rpc("grant.create", args).await
    }
    pub async fn session_grant_fetch(
        &self,
        public_key: HexBytes<32>,
    ) -> SessionResult<GrantResult> {
        self.session_rpc("grant.fetch", KeyArgs { public_key })
            .await
    }
    pub async fn session_challenge(&self, args: &ChallengeArgs) -> SessionResult<ChallengeResult> {
        let args = match args {
            ChallengeArgs::Key(k) => serde_json::to_value(k),
            ChallengeArgs::Record(r) => serde_json::to_value(r),
        }
        .map_err(|_| SessionFailure::InvalidResponse)?;
        let mut body: serde_json::Value = self.session_rpc("challenge", args).await?;
        let wake_error = body
            .as_object_mut()
            .and_then(|o| o.remove("wake_error"))
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| SessionFailure::InvalidResponse)?;
        let transcript =
            serde_json::from_value(body).map_err(|_| SessionFailure::InvalidResponse)?;
        Ok(ChallengeResult {
            transcript,
            wake_error,
        })
    }
    /// The BUS-016 key selector carries the fixed `enrol` tag on the wire.
    /// This is not a claim about the returned proof purpose: noded derives that
    /// from the record. Callers must independently expect enrol or resume in sign().
    pub async fn session_challenge_key(
        &self,
        public_key: HexBytes<32>,
    ) -> SessionResult<ChallengeResult> {
        self.session_challenge(&ChallengeArgs::Key(KeyChallenge {
            public_key,
            purpose: Purpose::Enrol,
        }))
        .await
    }
    pub async fn session_prove(&self, args: &ProveArgs) -> SessionResult<RecordResult> {
        self.session_rpc("prove", args).await
    }
    pub async fn session_renew(&self, target: RecordRef) -> SessionResult<RecordResult> {
        self.session_rpc("renew", TargetArgs { target }).await
    }
    pub async fn session_revoke(&self, target: RecordRef) -> SessionResult<RevokeResult> {
        self.session_rpc("revoke", TargetArgs { target }).await
    }
    pub async fn session_list(&self) -> SessionResult<ListResult> {
        self.session_rpc("list", serde_json::json!({})).await
    }
    /// Owner-UID scoped single-record read, independent of diagnostic list caps.
    pub async fn session_self(&self, record_id: HexBytes<16>) -> SessionResult<RecordResult> {
        self.session_rpc("self", SelfArgs { record_id }).await
    }
    /// Captures request-start CLOCK_BOOTTIME internally. Gaps invalidate results.
    pub async fn session_lease_check(&self, target: RecordRef) -> SessionResult<Deadline> {
        // This handle owns one transport and does not reconnect transparently.
        // Bind the check to its broker epoch and connection, not just a ref.
        // That context is fixed for the connection, so it costs one hello per
        // connection rather than one per check.
        let context = self.session_context().await?;
        let start = boottime_ms()?;
        let result: LeaseResult = self
            .session_rpc(
                "lease.check",
                TargetArgs {
                    target: target.clone(),
                },
            )
            .await?;
        let deadline = Deadline {
            target,
            broker_epoch: context.broker_epoch,
            connection_id: context.connection_id,
            expires_ms: start.saturating_add(result.lease_remaining_ms.0),
        };
        if !deadline.is_live(&context)? {
            return Err(SessionFailure::LeaseExpired);
        }
        Ok(deadline)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn deadline_refuses_broker_restart_and_same_epoch_reconnection() {
        let current = Hello {
            broker_epoch: HexBytes([1; 16]),
            connection_id: HexBytes([2; 16]),
        };
        let deadline = Deadline {
            target: RecordRef {
                record_id: HexBytes([3; 16]),
                incarnation: HexBytes([4; 16]),
                binding_generation: DecimalU64(1),
            },
            broker_epoch: current.broker_epoch,
            connection_id: current.connection_id,
            expires_ms: u64::MAX,
        };
        assert!(deadline.is_live(&current).unwrap());
        let restarted = Hello {
            broker_epoch: HexBytes([5; 16]),
            ..current.clone()
        };
        assert!(!deadline.is_live(&restarted).unwrap());
        let reconnected = Hello {
            connection_id: HexBytes([6; 16]),
            ..current.clone()
        };
        assert!(!deadline.is_live(&reconnected).unwrap());
        let expired = Deadline {
            expires_ms: 0,
            ..deadline
        };
        assert!(!expired.is_live(&current).unwrap());
    }
}
