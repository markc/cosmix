//! Optional compositor-corner Bus ingress for the layer host.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use calloop::channel::{Channel, SyncSender, sync_channel};
use cosmix_client::{
    ConnState, IncomingCommand, RegistrationRejected, SupervisedClient, SupervisedError,
};
use cosmix_shell::core::{Corner, CornerEvent, CornerTrigger, OutputKey};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const SERVICE: &str = "quoin-corners";
const CHANNEL_CAPACITY: usize = 64;
const MAX_BODY_BYTES: usize = 4 * 1024;
const MAX_DWELL_MS: u64 = 5_000;
const PROPS_GET_COMMAND: &str = "comp.props.get";
type OutputMap = BTreeMap<String, OutputKey>;
type OutputRefresh = (u64, Result<OutputMap, String>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CornerIngress {
    Event {
        output: OutputKey,
        epoch: u64,
        event: CornerEvent,
    },
    Reset {
        epoch: u64,
    },
    Disabled {
        epoch: u64,
    },
}

impl CornerIngress {
    pub const fn epoch(&self) -> u64 {
        match self {
            Self::Event { epoch, .. } | Self::Reset { epoch } | Self::Disabled { epoch } => *epoch,
        }
    }
}

pub(crate) struct CornerBusHandle {
    shutdown: Arc<AtomicBool>,
    ack: mpsc::Receiver<()>,
    selected_tx: UnboundedSender<(OutputKey, u64)>,
    epoch: Arc<AtomicU64>,
}

impl CornerBusHandle {
    pub fn select_output(&self, output: OutputKey) -> u64 {
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.selected_tx.send((output, epoch));
        epoch
    }

    pub fn shutdown(self) -> bool {
        self.shutdown.store(true, Ordering::Release);
        self.ack.recv_timeout(Duration::from_millis(300)).is_ok()
    }
}

pub(crate) struct CornerBusStart {
    pub handle: CornerBusHandle,
    pub channel: Channel<CornerIngress>,
    pub overflowed: Arc<AtomicBool>,
    pub epoch: Arc<AtomicU64>,
}

pub(crate) fn gate_ingress(
    event: CornerIngress,
    overflowed: &AtomicBool,
    shared_epoch: &AtomicU64,
    accepted_epoch: &mut u64,
) -> (bool, Option<CornerIngress>) {
    let reset = overflowed.swap(false, Ordering::AcqRel);
    if reset {
        *accepted_epoch = shared_epoch.load(Ordering::Acquire);
    }
    let accepted = (event.epoch() >= *accepted_epoch).then_some(event);
    (reset, accepted)
}

pub(crate) fn start(comp_service: String, selected: OutputKey) -> CornerBusStart {
    let (sender, channel) = sync_channel(CHANNEL_CAPACITY);
    let overflowed = Arc::new(AtomicBool::new(false));
    let epoch = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (ack_tx, ack) = mpsc::sync_channel(1);
    let (selected_tx, selected_rx) = unbounded_channel();
    let worker_shutdown = shutdown.clone();
    let worker_overflowed = overflowed.clone();
    let worker_epoch = epoch.clone();
    thread::Builder::new()
        .name("quoin-corner-bus".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(runtime) = runtime {
                runtime.block_on(worker(
                    comp_service,
                    selected,
                    selected_rx,
                    sender,
                    worker_overflowed,
                    worker_epoch,
                    worker_shutdown,
                ));
            }
            let _ = ack_tx.try_send(());
        })
        .expect("corner Bus worker thread must spawn");
    CornerBusStart {
        handle: CornerBusHandle {
            shutdown,
            ack,
            selected_tx,
            epoch: epoch.clone(),
        },
        channel,
        overflowed,
        epoch,
    }
}

#[derive(Clone)]
struct Topics {
    entered: String,
    left: String,
    output: String,
}

impl Topics {
    fn new(service: &str) -> Self {
        Self {
            entered: format!("{service}.corner.entered"),
            left: format!("{service}.corner.left"),
            output: format!("{service}.output.changed"),
        }
    }

    fn expected_command(&self, topic: &str) -> Option<&'static str> {
        if topic == self.entered {
            Some("corner.entered")
        } else if topic == self.left {
            Some("corner.left")
        } else if topic == self.output {
            Some("output.changed")
        } else {
            None
        }
    }

    fn subscriptions(&self) -> [&str; 3] {
        [&self.entered, &self.left, &self.output]
    }
}

trait CornerBusSession {
    async fn subscribe(&self, topic: &str) -> Result<(), String>;
    async fn outputs(&self, service: &str) -> Result<BTreeMap<String, OutputKey>, String>;
}

impl CornerBusSession for SupervisedClient {
    async fn subscribe(&self, topic: &str) -> Result<(), String> {
        self.subscribe_topic(topic)
            .await
            .map_err(|error| error.to_string())
    }

    async fn outputs(&self, service: &str) -> Result<BTreeMap<String, OutputKey>, String> {
        read_outputs(self, service).await
    }
}

async fn bootstrap<S: CornerBusSession>(
    session: &S,
    topics: &Topics,
    service: &str,
) -> Result<BTreeMap<String, OutputKey>, String> {
    for topic in topics.subscriptions() {
        session.subscribe(topic).await?;
    }
    session.outputs(service).await
}

struct WorkerState {
    topics: Topics,
    selected: OutputKey,
    outputs: Option<BTreeMap<String, OutputKey>>,
    map_valid: bool,
    queued: VecDeque<DecodedCorner>,
    engaged: BTreeSet<(String, Corner)>,
    diagnostics: u64,
    epoch: u64,
}

impl WorkerState {
    fn new(service: &str, selected: OutputKey) -> Self {
        Self {
            topics: Topics::new(service),
            selected,
            outputs: None,
            map_valid: false,
            queued: VecDeque::new(),
            engaged: BTreeSet::new(),
            diagnostics: 0,
            epoch: 0,
        }
    }

    fn reset(
        &mut self,
        sender: &SyncSender<CornerIngress>,
        overflowed: &AtomicBool,
        shared_epoch: &AtomicU64,
    ) {
        self.engaged.clear();
        self.queued.clear();
        send(
            sender,
            CornerIngress::Reset { epoch: self.epoch },
            overflowed,
            &mut self.epoch,
            shared_epoch,
        );
    }

    fn install_outputs(
        &mut self,
        next: BTreeMap<String, OutputKey>,
        sender: &SyncSender<CornerIngress>,
        overflowed: &AtomicBool,
        shared_epoch: &AtomicU64,
    ) {
        let invalid = self
            .engaged
            .iter()
            .filter(|(slug, _)| {
                next.get(slug) != self.outputs.as_ref().and_then(|current| current.get(slug))
            })
            .cloned()
            .collect::<Vec<_>>();
        for (slug, corner) in invalid {
            let output = self
                .outputs
                .as_ref()
                .and_then(|current| current.get(&slug))
                .cloned();
            self.engaged.remove(&(slug, corner));
            if let Some(output) = output {
                send(
                    sender,
                    CornerIngress::Event {
                        output,
                        epoch: self.epoch,
                        event: CornerEvent::Left { corner },
                    },
                    overflowed,
                    &mut self.epoch,
                    shared_epoch,
                );
            }
        }
        self.outputs = Some(next);
        self.map_valid = true;
        while let Some(event) = self.queued.pop_front() {
            self.apply_corner(event, sender, overflowed, shared_epoch);
        }
    }

    fn decode_and_apply(
        &mut self,
        command: IncomingCommand,
        sender: &SyncSender<CornerIngress>,
        overflowed: &AtomicBool,
        shared_epoch: &AtomicU64,
    ) -> DecodeAction {
        match decode(&self.topics, &command) {
            Ok(Decoded::Corner(corner)) => {
                if !self.map_valid {
                    if self.queued.len() == CHANNEL_CAPACITY {
                        self.reset(sender, overflowed, shared_epoch);
                        overflowed.store(true, Ordering::Release);
                    } else {
                        self.queued.push_back(corner);
                    }
                } else {
                    self.apply_corner(corner, sender, overflowed, shared_epoch);
                }
                DecodeAction::None
            }
            Ok(Decoded::Refresh) => {
                self.map_valid = false;
                self.reset(sender, overflowed, shared_epoch);
                DecodeAction::Refresh
            }
            Ok(Decoded::Gap) => {
                self.map_valid = false;
                self.reset(sender, overflowed, shared_epoch);
                DecodeAction::Refresh
            }
            Err(error) => {
                self.diagnostics = self.diagnostics.saturating_add(1);
                if self.diagnostics.is_power_of_two() {
                    tracing::warn!(event = "quoin_corner_decode_rejected", count = self.diagnostics, reason = %error);
                }
                DecodeAction::None
            }
        }
    }

    fn apply_corner(
        &mut self,
        event: DecodedCorner,
        sender: &SyncSender<CornerIngress>,
        overflowed: &AtomicBool,
        shared_epoch: &AtomicU64,
    ) {
        let Some(output) = self
            .outputs
            .as_ref()
            .and_then(|map| map.get(&event.output))
            .cloned()
        else {
            return;
        };
        if output != self.selected {
            return;
        }
        let key = (event.output, event.corner);
        let changed = if event.entered {
            self.engaged.insert(key)
        } else {
            self.engaged.remove(&key)
        };
        if !changed {
            return;
        }
        let event = if event.entered {
            CornerEvent::Entered {
                corner: event.corner,
                dwell: Duration::from_millis(event.dwell_ms),
                trigger: CornerTrigger::Compositor,
            }
        } else {
            CornerEvent::Left {
                corner: event.corner,
            }
        };
        send(
            sender,
            CornerIngress::Event {
                output,
                epoch: self.epoch,
                event,
            },
            overflowed,
            &mut self.epoch,
            shared_epoch,
        );
        if overflowed.load(Ordering::Acquire) {
            self.engaged.clear();
            self.queued.clear();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecodeAction {
    None,
    Refresh,
}

enum ConnectOutcome<T> {
    Connected(T),
    Rejected,
    Shutdown,
}

async fn connect_optional<T, E, F, Fut, P>(
    shutdown: &AtomicBool,
    mut connect: F,
    rejected: P,
) -> ConnectOutcome<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    P: Fn(&E) -> bool,
{
    let mut backoff = Duration::from_millis(100);
    loop {
        if shutdown.load(Ordering::Acquire) {
            return ConnectOutcome::Shutdown;
        }
        let connecting = connect();
        tokio::pin!(connecting);
        match tokio::select! {
            connected = &mut connecting => Some(connected),
            _ = wait_shutdown(shutdown) => None,
        } {
            Some(Ok(client)) => return ConnectOutcome::Connected(client),
            Some(Err(error)) if rejected(&error) => return ConnectOutcome::Rejected,
            Some(Err(_)) => {
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {},
                    _ = wait_shutdown(shutdown) => return ConnectOutcome::Shutdown,
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
            None => return ConnectOutcome::Shutdown,
        }
    }
}

#[derive(Default)]
struct RefreshGate {
    generation: u64,
    in_flight: bool,
    pending: bool,
}

impl RefreshGate {
    fn invalidate(&mut self) -> Option<u64> {
        self.generation = self.generation.wrapping_add(1);
        if self.in_flight {
            self.pending = true;
            None
        } else {
            self.in_flight = true;
            Some(self.generation)
        }
    }

    fn complete(&mut self, completed_generation: u64) -> (bool, Option<u64>) {
        self.in_flight = false;
        let install = completed_generation == self.generation;
        let follow_up = if self.pending {
            self.pending = false;
            self.in_flight = true;
            Some(self.generation)
        } else {
            None
        };
        (install, follow_up)
    }
}

async fn worker(
    comp_service: String,
    selected: OutputKey,
    mut selected_rx: UnboundedReceiver<(OutputKey, u64)>,
    sender: SyncSender<CornerIngress>,
    overflowed: Arc<AtomicBool>,
    shared_epoch: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
) {
    let topics = Topics::new(&comp_service);
    let url = cosmix_config::client_helpers::resolve_noded_url();
    while !shutdown.load(Ordering::Acquire) {
        let client = match connect_optional(
            &shutdown,
            || {
                SupervisedClient::connect_options(SERVICE, &url)
                    .fatal_on_registration_rejection(true)
                    .connect()
            },
            registration_rejected,
        )
        .await
        {
            ConnectOutcome::Connected(client) => Arc::new(client),
            ConnectOutcome::Rejected => {
                let mut epoch = shared_epoch.load(Ordering::Acquire);
                send(
                    &sender,
                    CornerIngress::Disabled { epoch },
                    &overflowed,
                    &mut epoch,
                    &shared_epoch,
                );
                return;
            }
            ConnectOutcome::Shutdown => return,
        };
        let Some(mut incoming) = client.incoming() else {
            client.shutdown().await;
            continue;
        };
        let mut state = WorkerState::new(&comp_service, selected.clone());
        state.epoch = shared_epoch.load(Ordering::Acquire);
        match bootstrap(client.as_ref(), &topics, &comp_service).await {
            Ok(outputs) => state.install_outputs(outputs, &sender, &overflowed, &shared_epoch),
            Err(_) => {
                client.shutdown().await;
                continue;
            }
        }
        let mut connection_state = client.subscribe_state();
        let mut generation = client.connection_generation();
        let (refresh_tx, mut refresh_rx) = unbounded_channel();
        let mut refresh_gate = RefreshGate::default();
        loop {
            if shutdown.load(Ordering::Acquire) {
                state.reset(&sender, &overflowed, &shared_epoch);
                let _ = tokio::time::timeout(Duration::from_millis(250), client.shutdown()).await;
                return;
            }
            tokio::select! {
                command = incoming.recv() => {
                    let Some(command) = command else { break; };
                    if state.decode_and_apply(command, &sender, &overflowed, &shared_epoch) == DecodeAction::Refresh
                        && let Some(generation) = refresh_gate.invalidate()
                    {
                        start_output_refresh(
                            client.clone(),
                            comp_service.clone(),
                            generation,
                            refresh_tx.clone(),
                        );
                    }
                }
                refreshed = refresh_rx.recv() => {
                    let Some((completed_generation, result)) = refreshed else { break; };
                    let (install, follow_up) = refresh_gate.complete(completed_generation);
                    if install && let Ok(outputs) = result
                    {
                        state.install_outputs(outputs, &sender, &overflowed, &shared_epoch);
                    }
                    if let Some(generation) = follow_up {
                        start_output_refresh(
                            client.clone(),
                            comp_service.clone(),
                            generation,
                            refresh_tx.clone(),
                        );
                    }
                }
                changed = connection_state.changed() => {
                    if changed.is_err() { break; }
                    match *connection_state.borrow_and_update() {
                        ConnState::Disconnected => {
                            state.map_valid = false;
                            state.reset(&sender, &overflowed, &shared_epoch);
                        }
                        ConnState::Connected if take_new_generation(&mut generation, client.connection_generation()) => {
                            state.map_valid = false;
                            state.reset(&sender, &overflowed, &shared_epoch);
                            if let Some(generation) = refresh_gate.invalidate() {
                                start_output_refresh(
                                    client.clone(),
                                    comp_service.clone(),
                                    generation,
                                    refresh_tx.clone(),
                                );
                            }
                        }
                        ConnState::Fatal => {
                            send(
                                &sender,
                                CornerIngress::Disabled { epoch: state.epoch },
                                &overflowed,
                                &mut state.epoch,
                                &shared_epoch,
                            );
                            return;
                        }
                        _ => {}
                    }
                }
                selected = selected_rx.recv() => {
                    if let Some((selected, epoch)) = selected {
                        state.selected = selected;
                        state.epoch = state.epoch.max(epoch);
                        state.map_valid = false;
                        state.reset(&sender, &overflowed, &shared_epoch);
                        if let Some(generation) = refresh_gate.invalidate() {
                            start_output_refresh(
                                client.clone(),
                                comp_service.clone(),
                                generation,
                                refresh_tx.clone(),
                            );
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
        state.reset(&sender, &overflowed, &shared_epoch);
        client.shutdown().await;
    }
}

fn start_output_refresh(
    client: Arc<SupervisedClient>,
    service: String,
    generation: u64,
    sender: UnboundedSender<OutputRefresh>,
) {
    tokio::spawn(async move {
        let result = read_outputs(&client, &service).await;
        let _ = sender.send((generation, result));
    });
}

async fn wait_shutdown(shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn registration_rejected(error: &SupervisedError) -> bool {
    matches!(error, SupervisedError::InitialConnectFailed { source, .. } if is_registration_rejection(source.as_ref()))
}

fn is_registration_rejection(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<RegistrationRejected>().is_some()
}

fn take_new_generation(previous: &mut u64, current: u64) -> bool {
    if *previous == current {
        false
    } else {
        *previous = current;
        true
    }
}

async fn read_outputs(
    client: &SupervisedClient,
    service: &str,
) -> Result<BTreeMap<String, OutputKey>, String> {
    let value = client
        .call(service, PROPS_GET_COMMAND, json!({"path": "outputs"}))
        .await
        .map_err(|error| error.to_string())?;
    parse_outputs(&value)
}

fn parse_outputs(value: &Value) -> Result<BTreeMap<String, OutputKey>, String> {
    let rows = value.get("outputs").unwrap_or(value);
    let rows = rows.as_object().ok_or("outputs is not an object")?;
    let mut outputs = BTreeMap::new();
    for (slug, row) in rows {
        if !valid_slug(slug) {
            return Err("invalid output slug".to_owned());
        }
        let name = row
            .as_object()
            .and_then(|row| row.get("name"))
            .and_then(Value::as_str)
            .ok_or("output row has no name")?;
        outputs.insert(
            slug.clone(),
            OutputKey::new(name).map_err(|error| error.to_string())?,
        );
    }
    Ok(outputs)
}

fn send(
    sender: &SyncSender<CornerIngress>,
    event: CornerIngress,
    overflowed: &AtomicBool,
    epoch: &mut u64,
    shared_epoch: &AtomicU64,
) {
    if sender.try_send(event).is_err() {
        *epoch = shared_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        overflowed.store(true, Ordering::Release);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DecodedCorner {
    output: String,
    corner: Corner,
    dwell_ms: u64,
    entered: bool,
}

enum Decoded {
    Corner(DecodedCorner),
    Refresh,
    Gap,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CornerBody {
    output: String,
    corner: String,
    dwell_ms: u64,
    event_seq: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeometryRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputBody {
    output: String,
    geometry: GeometryRect,
    usable: LogicalRect,
    event_seq: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GapBody {
    gap: bool,
    lost_count: u64,
    cause: String,
}

fn decode(topics: &Topics, command: &IncomingCommand) -> Result<Decoded, String> {
    if command.body.len() > MAX_BODY_BYTES {
        return Err("body too large".to_owned());
    }
    let topic = command.header("topic").ok_or("missing topic header")?;
    let expected = topics.expected_command(topic).ok_or("unexpected topic")?;
    if command.command != expected {
        return Err("topic/command mismatch".to_owned());
    }
    if let Ok(gap) = serde_json::from_str::<GapBody>(&command.body) {
        if gap.gap && matches!(gap.cause.as_str(), "outbox.overflow" | "publisher.loss") {
            let _ = gap.lost_count;
            return Ok(Decoded::Gap);
        }
        return Err("invalid gap".to_owned());
    }
    if expected == "output.changed" {
        let output: OutputBody =
            serde_json::from_str(&command.body).map_err(|error| error.to_string())?;
        if !valid_slug(&output.output)
            || output.geometry.width == 0
            || output.geometry.height == 0
            || !output.usable.x.is_finite()
            || !output.usable.y.is_finite()
            || !output.usable.width.is_finite()
            || !output.usable.height.is_finite()
            || output.usable.width <= 0.0
            || output.usable.height <= 0.0
        {
            return Err("invalid output body".to_owned());
        }
        let _ = (
            output.geometry.x,
            output.geometry.y,
            output.usable.x,
            output.usable.y,
            output.event_seq,
        );
        return Ok(Decoded::Refresh);
    }
    let body: CornerBody =
        serde_json::from_str(&command.body).map_err(|error| error.to_string())?;
    if !valid_slug(&body.output) || body.dwell_ms > MAX_DWELL_MS {
        return Err("invalid corner body".to_owned());
    }
    let corner = match body.corner.as_str() {
        "tl" => Corner::TopLeft,
        "bl" => Corner::BottomLeft,
        "br" => Corner::BottomRight,
        "tr" => Corner::TopRight,
        _ => return Err("invalid corner".to_owned()),
    };
    let _ = body.event_seq;
    Ok(Decoded::Corner(DecodedCorner {
        output: body.output,
        corner,
        dwell_ms: body.dwell_ms,
        entered: expected == "corner.entered",
    }))
}

fn valid_slug(slug: &str) -> bool {
    slug.strip_prefix("o_").is_some_and(|tail| {
        !tail.is_empty()
            && tail
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    struct FakeSession {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl CornerBusSession for FakeSession {
        async fn subscribe(&self, topic: &str) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("subscribe:{topic}"));
            Ok(())
        }

        async fn outputs(&self, service: &str) -> Result<BTreeMap<String, OutputKey>, String> {
            self.calls.lock().unwrap().push(format!("read:{service}"));
            Ok(BTreeMap::from([(
                "o_dp_1".to_owned(),
                OutputKey::new("DP-1").unwrap(),
            )]))
        }
    }

    fn incoming(topic: &str, command: &str, body: &str) -> IncomingCommand {
        IncomingCommand {
            from: "comp".to_owned(),
            command: command.to_owned(),
            id: None,
            args: Value::Null,
            body: body.to_owned(),
            headers: BTreeMap::from([("topic".to_owned(), topic.to_owned())]),
        }
    }

    #[test]
    fn strict_topic_command_body_and_size_validation() {
        let topics = Topics::new("comp-nested");
        let body = r#"{"output":"o_dp_1","corner":"tl","dwell_ms":200,"event_seq":9}"#;
        assert!(matches!(
            decode(
                &topics,
                &incoming("comp-nested.corner.entered", "corner.entered", body)
            ),
            Ok(Decoded::Corner(_))
        ));
        assert!(
            decode(
                &topics,
                &incoming("comp.corner.entered", "corner.entered", body)
            )
            .is_err()
        );
        assert!(
            decode(
                &topics,
                &incoming(
                    "comp-nested.corner.entered",
                    "comp-nested.corner.entered",
                    body
                )
            )
            .is_err()
        );
        assert!(
            decode(
                &topics,
                &incoming(
                    "comp-nested.corner.entered",
                    "corner.entered",
                    &"x".repeat(MAX_BODY_BYTES + 1)
                )
            )
            .is_err()
        );
        let unknown = r#"{"output":"o_dp_1","corner":"tl","dwell_ms":200,"event_seq":9,"extra":1}"#;
        assert!(
            decode(
                &topics,
                &incoming("comp-nested.corner.entered", "corner.entered", unknown)
            )
            .is_err()
        );
    }

    #[test]
    fn shape_amended_subscriptions_precede_the_initial_read() {
        let topics = Topics::new("comp-nested");
        let fake = FakeSession {
            calls: std::sync::Mutex::new(Vec::new()),
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(bootstrap(&fake, &topics, "comp-nested"))
            .unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            [
                "subscribe:comp-nested.corner.entered",
                "subscribe:comp-nested.corner.left",
                "subscribe:comp-nested.output.changed",
                "read:comp-nested",
            ]
        );
    }

    #[test]
    fn props_map_requires_slug_and_raw_name() {
        let map = parse_outputs(&json!({"o_dp_1":{"name":"DP-1"}})).unwrap();
        assert_eq!(map["o_dp_1"].as_str(), "DP-1");
        assert!(parse_outputs(&json!({"DP-1":{"name":"DP-1"}})).is_err());
        assert!(parse_outputs(&json!({"o_dp_1":{}})).is_err());
    }

    #[test]
    fn selected_raw_output_only_and_duplicates_are_noops() {
        let (sender, channel) = sync_channel(8);
        let overflow = AtomicBool::new(false);
        let epoch = AtomicU64::new(0);
        let mut state = WorkerState::new("comp", OutputKey::new("DP-1").unwrap());
        state.install_outputs(
            BTreeMap::from([
                ("o_dp_1".to_owned(), OutputKey::new("DP-1").unwrap()),
                ("o_hdmi_a_1".to_owned(), OutputKey::new("HDMI-A-1").unwrap()),
            ]),
            &sender,
            &overflow,
            &epoch,
        );
        let foreign = DecodedCorner {
            output: "o_hdmi_a_1".to_owned(),
            corner: Corner::TopLeft,
            dwell_ms: 10,
            entered: true,
        };
        state.apply_corner(foreign, &sender, &overflow, &epoch);
        assert!(channel.try_recv().is_err());
        let selected = DecodedCorner {
            output: "o_dp_1".to_owned(),
            corner: Corner::TopLeft,
            dwell_ms: 10,
            entered: true,
        };
        state.apply_corner(selected.clone(), &sender, &overflow, &epoch);
        state.apply_corner(selected, &sender, &overflow, &epoch);
        assert!(matches!(
            channel.try_recv(),
            Ok(CornerIngress::Event {
                event: CornerEvent::Entered { .. },
                ..
            })
        ));
        assert!(channel.try_recv().is_err());
    }

    #[test]
    fn queued_corner_waits_for_map_and_remap_synthesizes_left() {
        let (sender, channel) = sync_channel(8);
        let overflow = AtomicBool::new(false);
        let epoch = AtomicU64::new(0);
        let mut state = WorkerState::new("comp", OutputKey::new("DP-1").unwrap());
        state.queued.push_back(DecodedCorner {
            output: "o_dp_1".to_owned(),
            corner: Corner::TopLeft,
            dwell_ms: 20,
            entered: true,
        });
        assert!(channel.try_recv().is_err());
        state.install_outputs(
            BTreeMap::from([("o_dp_1".to_owned(), OutputKey::new("DP-1").unwrap())]),
            &sender,
            &overflow,
            &epoch,
        );
        assert!(matches!(
            channel.try_recv(),
            Ok(CornerIngress::Event {
                event: CornerEvent::Entered { .. },
                ..
            })
        ));
        state.install_outputs(
            BTreeMap::from([("o_dp_1".to_owned(), OutputKey::new("DP-2").unwrap())]),
            &sender,
            &overflow,
            &epoch,
        );
        assert!(matches!(
            channel.try_recv(),
            Ok(CornerIngress::Event {
                event: CornerEvent::Left { .. },
                ..
            })
        ));
    }

    #[test]
    fn removed_output_cannot_reveal_after_atomic_map_replacement() {
        let (sender, channel) = sync_channel(8);
        let overflow = AtomicBool::new(false);
        let epoch = AtomicU64::new(0);
        let mut state = WorkerState::new("comp", OutputKey::new("DP-1").unwrap());
        state.install_outputs(
            BTreeMap::from([("o_dp_1".to_owned(), OutputKey::new("DP-1").unwrap())]),
            &sender,
            &overflow,
            &epoch,
        );
        state.install_outputs(BTreeMap::new(), &sender, &overflow, &epoch);
        state.apply_corner(
            DecodedCorner {
                output: "o_dp_1".to_owned(),
                corner: Corner::TopLeft,
                dwell_ms: 20,
                entered: true,
            },
            &sender,
            &overflow,
            &epoch,
        );
        assert!(channel.try_recv().is_err());
    }

    #[test]
    fn gap_is_a_reset_and_refresh_not_sequence_ordering() {
        let topics = Topics::new("comp");
        let gap = incoming(
            "comp.corner.left",
            "corner.left",
            r#"{"gap":true,"lost_count":2,"cause":"outbox.overflow"}"#,
        );
        assert!(matches!(decode(&topics, &gap), Ok(Decoded::Gap)));
        let publisher_loss = incoming(
            "comp.corner.entered",
            "corner.entered",
            r#"{"gap":true,"lost_count":3,"cause":"publisher.loss"}"#,
        );
        assert!(matches!(decode(&topics, &publisher_loss), Ok(Decoded::Gap)));
    }

    #[test]
    fn output_notice_is_strict_and_requests_refresh() {
        let topics = Topics::new("comp");
        let body = r#"{"output":"o_dp_1","geometry":{"x":0,"y":0,"width":1000,"height":800},"usable":{"x":0.0,"y":0.0,"width":1000.0,"height":760.0},"event_seq":22}"#;
        assert!(matches!(
            decode(
                &topics,
                &incoming("comp.output.changed", "output.changed", body)
            ),
            Ok(Decoded::Refresh)
        ));
    }

    #[test]
    fn output_notice_invalidates_map_and_clears_engagements() {
        let (sender, channel) = sync_channel(8);
        let overflow = AtomicBool::new(false);
        let epoch = AtomicU64::new(0);
        let mut state = WorkerState::new("comp", OutputKey::new("DP-1").unwrap());
        state.install_outputs(
            BTreeMap::from([("o_dp_1".to_owned(), OutputKey::new("DP-1").unwrap())]),
            &sender,
            &overflow,
            &epoch,
        );
        state.engaged.insert(("o_dp_1".into(), Corner::TopLeft));

        let action = state.decode_and_apply(
            incoming(
                "comp.output.changed",
                "output.changed",
                r#"{"output":"o_dp_1","geometry":{"x":0,"y":0,"width":1000,"height":800},"usable":{"x":0.0,"y":0.0,"width":1000.0,"height":760.0},"event_seq":22}"#,
            ),
            &sender,
            &overflow,
            &epoch,
        );

        assert_eq!(action, DecodeAction::Refresh);
        assert!(!state.map_valid);
        assert!(state.engaged.is_empty());
        assert!(matches!(
            channel.try_recv(),
            Ok(CornerIngress::Reset { .. })
        ));
    }

    #[test]
    fn output_notices_coalesce_to_one_follow_up_while_a_read_is_in_flight() {
        let mut gate = RefreshGate::default();
        assert_eq!(gate.invalidate(), Some(1));
        assert_eq!(gate.invalidate(), None);
        assert_eq!(gate.invalidate(), None);
        assert_eq!(gate.complete(1), (false, Some(3)));
        assert_eq!(gate.complete(3), (true, None));
    }

    #[test]
    fn disconnect_reset_clears_engagement_once() {
        let (sender, channel) = sync_channel(8);
        let overflow = AtomicBool::new(false);
        let epoch = AtomicU64::new(0);
        let mut state = WorkerState::new("comp", OutputKey::new("DP-1").unwrap());
        state.engaged.insert(("o_dp_1".to_owned(), Corner::TopLeft));
        state.reset(&sender, &overflow, &epoch);
        state.reset(&sender, &overflow, &epoch);
        assert!(state.engaged.is_empty());
        assert_eq!(
            channel.try_recv().unwrap(),
            CornerIngress::Reset { epoch: 0 }
        );
        assert_eq!(
            channel.try_recv().unwrap(),
            CornerIngress::Reset { epoch: 0 }
        );
    }

    #[test]
    fn connection_generation_refreshes_exactly_once() {
        let mut generation = 4;
        assert!(!take_new_generation(&mut generation, 4));
        assert!(take_new_generation(&mut generation, 5));
        assert!(!take_new_generation(&mut generation, 5));
    }

    #[test]
    fn registration_rejection_is_distinct_from_optional_broker_absence() {
        let rejection = RegistrationRejected {
            rc: 10,
            message: "collision".to_owned(),
        };
        assert!(is_registration_rejection(&rejection));
        let absence = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "absent");
        assert!(!is_registration_rejection(&absence));
    }

    #[test]
    fn full_channel_sets_fail_safe_barrier_and_never_replays_stale_enter() {
        let (sender, channel) = sync_channel(1);
        let overflow = AtomicBool::new(false);
        let epoch = AtomicU64::new(0);
        let mut worker_epoch = 0;
        send(
            &sender,
            CornerIngress::Event {
                output: OutputKey::new("DP-1").unwrap(),
                epoch: 0,
                event: CornerEvent::Entered {
                    corner: Corner::TopLeft,
                    dwell: Duration::ZERO,
                    trigger: CornerTrigger::Compositor,
                },
            },
            &overflow,
            &mut worker_epoch,
            &epoch,
        );
        send(
            &sender,
            CornerIngress::Event {
                output: OutputKey::new("DP-1").unwrap(),
                epoch: 0,
                event: CornerEvent::Left {
                    corner: Corner::TopLeft,
                },
            },
            &overflow,
            &mut worker_epoch,
            &epoch,
        );
        assert!(overflow.load(Ordering::Acquire));
        assert_eq!(worker_epoch, 1);
        let mut accepted_epoch = 0;
        let (reset, accepted) = gate_ingress(
            channel.try_recv().unwrap(),
            &overflow,
            &epoch,
            &mut accepted_epoch,
        );
        assert!(reset);
        assert!(accepted.is_none());
        assert_eq!(accepted_epoch, 1);
    }

    #[test]
    fn selection_epoch_rejects_an_old_event_even_when_the_raw_name_is_reused() {
        let overflow = AtomicBool::new(false);
        let epoch = AtomicU64::new(1);
        let mut accepted_epoch = 1;
        let old = CornerIngress::Event {
            output: OutputKey::new("DP-1").unwrap(),
            epoch: 0,
            event: CornerEvent::Entered {
                corner: Corner::TopLeft,
                dwell: Duration::ZERO,
                trigger: CornerTrigger::Compositor,
            },
        };
        let (reset, accepted) = gate_ingress(old, &overflow, &epoch, &mut accepted_epoch);
        assert!(!reset);
        assert!(accepted.is_none());
    }

    #[test]
    fn fake_absent_broker_retry_and_shutdown_ack_are_bounded() {
        let shutdown = AtomicBool::new(false);
        let attempts = std::cell::Cell::new(0_u8);
        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(connect_optional(
                &shutdown,
                || {
                    attempts.set(attempts.get() + 1);
                    shutdown.store(true, Ordering::Release);
                    std::future::ready(Err::<(), ()>(()))
                },
                |_| false,
            ));
        assert!(matches!(outcome, ConnectOutcome::Shutdown));
        assert_eq!(attempts.get(), 1);

        let (ack_tx, ack) = mpsc::sync_channel(1);
        ack_tx.send(()).unwrap();
        let (selected_tx, _selected_rx) = unbounded_channel();
        let handle = CornerBusHandle {
            shutdown: Arc::new(AtomicBool::new(false)),
            ack,
            selected_tx,
            epoch: Arc::new(AtomicU64::new(0)),
        };
        let before = Instant::now();
        assert!(handle.shutdown());
        assert!(before.elapsed() <= Duration::from_millis(300));
    }
}
