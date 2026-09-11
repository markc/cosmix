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
/// Discard on connection/epoch loss or a lifecycle gap.
#[derive(Debug, Clone)]
pub struct Deadline {
    target: RecordRef,
    expires_ms: u64,
}

impl Deadline {
    pub fn target(&self) -> &RecordRef {
        &self.target
    }

    pub fn is_live(&self) -> SessionResult<bool> {
        Ok(boottime_ms()? < self.expires_ms)
    }
}

fn boottime_ms() -> SessionResult<u64> {
    #[cfg(target_os = "linux")]
    {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: ts is a valid writable timespec.
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
            return Err(SessionFailure::Transport(std::io::Error::last_os_error().into()));
        }
        Ok((ts.tv_sec as u64).saturating_mul(1000).saturating_add(ts.tv_nsec as u64 / 1_000_000))
    }
    #[cfg(not(target_os = "linux"))]
    Err(SessionFailure::Transport(anyhow::anyhow!("CLOCK_BOOTTIME is unavailable")))
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
        if p.unix_uid != expected.unix_uid
            || p.parent_key_hash != expected.parent_key_hash
            || p.pane_id != expected.pane_id
            || expected.pane_high_water.is_some_and(|high| {
                p.pane_generation.is_none_or(|generation| generation.0 < high.0)
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
    pub async fn session_allocate(
        &self,
        key: &SigningKey,
        policy: Policy,
    ) -> SessionResult<RecordResult> {
        let hello = self.session_hello().await?;
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
    /// Captures request-start CLOCK_BOOTTIME internally. Gaps invalidate results.
    pub async fn session_lease_check(&self, target: RecordRef) -> SessionResult<Deadline> {
        let start = boottime_ms()?;
        let result: LeaseResult = self.session_rpc("lease.check", TargetArgs { target: target.clone() }).await?;
        let deadline = Deadline {
            target,
            expires_ms: start.saturating_add(result.lease_remaining_ms.0),
        };
        if !deadline.is_live()? {
            return Err(SessionFailure::LeaseExpired);
        }
        Ok(deadline)
    }
}
