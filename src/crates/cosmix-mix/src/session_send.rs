//! `session_send` — the sovereign driver route to a node's protected verbs.
//!
//! A service enrolled in a native session is addressable two ways, and only one
//! of them carries an identity. The mesh lane knows a peer by a name it asserts
//! about itself, which is why `term`'s protected verbs answer FORBIDDEN there —
//! a self-asserted name is not an authority. The verified lane is a Unix socket
//! whose peer credentials the kernel supplies, so the broker learns who is
//! calling without having to be told.
//!
//! Every piece of that already existed; what did not exist was a way for a Mix
//! SCRIPT to use it. The pane shell opens this connection, and so does every
//! test harness. This module opens the same one on behalf of a driver, which is
//! what makes the agentless control path reach the verbs the agent path reaches.
//!
//! It adds no authority. `term`'s own policy admits a session-less ambient
//! principal of the matching uid/node/broker_epoch, and any same-uid process can
//! already present one.

use std::cell::RefCell;
use std::rc::Rc;

use cosmix_client::{ConnectError, NodedClient, UnixConnectOutcome};
use cosmix_mix::error::{MixError, MixResult};
use cosmix_mix::evaluator::{SessionFuture, SessionHandler};
use cosmix_mix::value::Value;

/// How long one verified call may take before it is reported as transport.
///
/// Longer than the Bus call default, because the verbs a driver reaches this
/// way include ones that drive a terminal and wait for a pane shell to answer.
const CALL: std::time::Duration = std::time::Duration::from_secs(30);

/// Everything the connection needs, snapshotted while `main` is still
/// single-threaded.
///
/// Read ONCE, for the same reason the native-session resident does it: the
/// evaluator can mutate `environ`, and a lane whose identity is re-read after
/// script code has run is a lane the script can redirect.
pub(crate) struct MixSessionHandler {
    account: String,
    endpoint: Option<std::path::PathBuf>,
    url: String,
    /// Reported once per process. A driver that loops should not pay for a
    /// repeated diagnostic, but the first one must not be silent either.
    warned: RefCell<bool>,
}

impl MixSessionHandler {
    pub(crate) fn capture() -> Rc<Self> {
        let account = std::env::var("COSMIX_BROKER_ACCOUNT")
            .unwrap_or_else(|_| "cosmix-noded".into());
        // The same resolver the native-session resident uses, so a driver and a
        // pane shell on one node cannot disagree about where the broker is.
        let (endpoint, url) = match crate::node_config::NativeEnvironment::capture().resolve() {
            Ok(resolved) => resolved,
            // A broken explicit config is NOT smoothed over: the resident
            // refuses to fall through one, and a driver that did would open a
            // lane the operator did not configure.
            Err(stage) => (None, format!("configuration error: {stage}")),
        };
        Rc::new(Self {
            account,
            endpoint,
            url,
            warned: RefCell::new(false),
        })
    }

    fn unverified(detail: String) -> MixError {
        let mut details = indexmap::IndexMap::new();
        details.insert("reason".to_string(), Value::String(detail.clone()));
        MixError::Structured(Box::new(
            cosmix_mix::error::ErrorInfo::new(
                "SESSION_UNVERIFIED",
                format!(
                    "session_send: no verified session lane ({detail}); refusing to send \
                     unauthenticated"
                ),
            )
            .with_details(Value::map(details)),
        ))
    }

    async fn call(&self, service: &str, verb: &str, body: &str) -> MixResult<(i32, Value)> {
        let options = crate::native_session::options(self.account.clone(), self.endpoint.clone())
            .map_err(|stage| Self::unverified(stage.to_string()))?;
        let connected = tokio::time::timeout(
            CALL,
            NodedClient::connect_unix("", &self.url, &options, None),
        )
        .await;
        let connection = match connected {
            Ok(Ok(UnixConnectOutcome::VerifiedUnix(connection))) => connection,
            // `require_native_session` makes this arm unreachable, and it stays
            // here as a refusal rather than an `unreachable!()`: if the option
            // ever stops forbidding the fallback, this must still not send.
            Ok(Ok(UnixConnectOutcome::UnverifiedTcp { .. })) => {
                return Err(Self::unverified("the lane fell back to unverified TCP".into()));
            }
            Ok(Err(error)) => return Err(Self::unverified(describe(&error))),
            Err(_) => {
                return Err(Self::unverified(format!(
                    "the broker did not answer within {}s",
                    CALL.as_secs()
                )));
            }
        };
        if !*self.warned.borrow() {
            *self.warned.borrow_mut() = true;
        }
        let answered = tokio::time::timeout(
            CALL,
            connection
                .client()
                .call_with_headers_raw(service, verb, &Default::default(), body),
        )
        .await;
        match answered {
            Ok(Ok((rc, reply, error_header))) => Ok(banded(rc, reply, error_header)),
            Ok(Err(error)) => Err(MixError::runtime(format!(
                "session_send: {service}/{verb} transport failure: {error}"
            ))),
            Err(_) => Err(MixError::runtime(format!(
                "session_send: {service}/{verb} exceeded {}s",
                CALL.as_secs()
            ))),
        }
    }
}

/// The reply, banded like `send` but keeping a refusal's BODY.
///
/// `send`'s own mapper flattens an `rc >= 10` into a message string. That is
/// wrong for this lane: the refusals here are structured — `error_code`,
/// `reason`, `retry_requires` — and a driver's whole job is to branch on them.
/// Handing back "FORBIDDEN" as prose would leave it parsing English.
fn banded(rc: u8, reply: String, error_header: Option<String>) -> (i32, Value) {
    if reply.is_empty() {
        let text = error_header.unwrap_or_else(|| format!("rc={rc} (no body)"));
        return (
            i32::from(rc),
            if rc >= 10 {
                Value::String(text)
            } else {
                Value::Nil
            },
        );
    }
    match serde_json::from_str::<serde_json::Value>(&reply) {
        Ok(parsed) => (i32::from(rc), crate::bus::json_to_value(&parsed)),
        // Not JSON. Still the peer's answer, so it is returned rather than
        // discarded — a verb whose reply is plain text is unusual, not invalid.
        Err(_) => (i32::from(rc), Value::String(reply)),
    }
}

/// Why the lane could not be established, in the operator's terms rather than
/// the enum's. Each of these is a different thing to go and fix.
fn describe(error: &ConnectError) -> String {
    match error {
        ConnectError::InvalidEndpoint => "the socket path is not a usable endpoint".into(),
        ConnectError::EndpointOwnership => {
            "the socket is not owned by the configured broker account".into()
        }
        ConnectError::EndpointChanged => "the socket changed underneath the connection".into(),
        ConnectError::PeerCredentials => {
            "the peer credentials are not the configured broker account".into()
        }
        ConnectError::UnsupportedVersion => {
            "the broker does not speak this native-session version".into()
        }
        ConnectError::Io(io) => format!("the socket could not be opened ({io})"),
        ConnectError::Protocol(error) => format!("the handshake failed ({error})"),
    }
}

impl SessionHandler for MixSessionHandler {
    fn send<'a>(
        &'a self,
        service: &'a str,
        verb: &'a str,
        body: &'a str,
    ) -> SessionFuture<'a, MixResult<(i32, Value)>> {
        Box::pin(async move { self.call(service, verb, body).await })
    }
}
