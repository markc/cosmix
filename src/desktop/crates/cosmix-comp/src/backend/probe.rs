use std::{
    env,
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender},
    thread::{self, JoinHandle},
    time::Duration,
};

use cosmix_wgpu_dmabuf::{ManualVulkanRenderer, VulkanDrmAdapter, VulkanDrmProbe};
use smithay::{
    backend::{
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, primary_gpu},
    },
    reexports::{
        calloop::{EventLoop, channel},
        input::Libinput,
    },
};

use super::atomic_present::{
    AtomicAdmissionOutcome, AtomicAdmissionReport, AtomicCapabilityState, AtomicSurvivorStatus,
    probe_atomic_admission,
};
use super::kms::DeviceId;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DrmCard {
    pub(crate) device: DeviceId,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProtocolProbeFacts {
    session: ProbeStatus,
    session_seat: Option<String>,
    session_active: Option<bool>,
    session_source: ProbeStatus,
    udev_source: ProbeStatus,
    libinput_source: ProbeStatus,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ProbeStatus {
    available: bool,
    unavailable_reason: Option<String>,
}

impl ProbeStatus {
    fn available() -> Self {
        Self {
            available: true,
            unavailable_reason: None,
        }
    }

    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            unavailable_reason: Some(reason.into()),
        }
    }
}

enum SessionSetup {
    Attempt,
    #[cfg(test)]
    Unavailable(String),
}

enum ProbeCommand {
    Ping(SyncSender<()>),
    Shutdown,
}

#[derive(Default)]
struct ProbeLoopState {
    shutdown: bool,
    session_active: bool,
    session_events: u64,
    udev_events: u64,
    ignored_input_events: u64,
}

struct ProtocolProbeRuntime {
    facts: ProtocolProbeFacts,
    commands: channel::Sender<ProbeCommand>,
    thread: Option<JoinHandle<()>>,
}

impl ProtocolProbeRuntime {
    fn start(seat: &str) -> Result<Self, String> {
        Self::start_with_session_setup(seat, SessionSetup::Attempt)
    }

    fn start_with_session_setup(seat: &str, session_setup: SessionSetup) -> Result<Self, String> {
        let (commands, command_source) = channel::channel();
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let seat = seat.to_owned();
        let thread = thread::Builder::new()
            .name("cosmix-wayland".into())
            .spawn(move || {
                let result = build_protocol_probe(command_source, &seat, session_setup);
                let (mut event_loop, mut state, facts) = match result {
                    Ok(parts) => parts,
                    Err(error) => {
                        let _ = startup_sender.send(Err(error));
                        return;
                    }
                };
                if startup_sender.send(Ok(facts)).is_err() {
                    return;
                }
                while !state.shutdown {
                    if let Err(error) = event_loop.dispatch(None, &mut state) {
                        tracing::error!(%error, "KMS probe calloop stopped");
                        return;
                    }
                }
                tracing::debug!(
                    session_events = state.session_events,
                    udev_events = state.udev_events,
                    ignored_input_events = state.ignored_input_events,
                    "KMS protocol probe stopped"
                );
            })
            .map_err(|error| format!("failed to start KMS protocol probe thread: {error}"))?;

        match startup_receiver.recv_timeout(PROBE_TIMEOUT) {
            Ok(Ok(facts)) => Ok(Self {
                facts,
                commands,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                let _ = commands.send(ProbeCommand::Shutdown);
                let _ = thread.join();
                Err(format!("KMS protocol probe startup timed out: {error}"))
            }
        }
    }

    fn check_liveness(&self) -> Result<(), String> {
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        self.commands
            .send(ProbeCommand::Ping(reply_sender))
            .map_err(|_| "KMS protocol probe calloop disconnected".to_string())?;
        reply_receiver
            .recv_timeout(PROBE_TIMEOUT)
            .map_err(|error| format!("KMS protocol probe liveness check timed out: {error}"))
    }
}

impl Drop for ProtocolProbeRuntime {
    fn drop(&mut self) {
        let _ = self.commands.send(ProbeCommand::Shutdown);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("KMS protocol probe thread panicked during shutdown");
        }
    }
}

fn build_protocol_probe(
    command_source: channel::Channel<ProbeCommand>,
    seat: &str,
    session_setup: SessionSetup,
) -> Result<
    (
        EventLoop<'static, ProbeLoopState>,
        ProbeLoopState,
        ProtocolProbeFacts,
    ),
    String,
> {
    let event_loop = EventLoop::try_new()
        .map_err(|error| format!("KMS probe calloop construction failed: {error}"))?;
    event_loop
        .handle()
        .insert_source(
            command_source,
            |event, (), state: &mut ProbeLoopState| match event {
                channel::Event::Msg(ProbeCommand::Ping(reply)) => {
                    let _ = reply.send(());
                }
                channel::Event::Msg(ProbeCommand::Shutdown) | channel::Event::Closed => {
                    state.shutdown = true;
                }
            },
        )
        .map_err(|error| format!("probe-control calloop registration failed: {error}"))?;

    let udev_source = match UdevBackend::new(seat) {
        Ok(udev) => {
            match event_loop
                .handle()
                .insert_source(udev, |_, (), state: &mut ProbeLoopState| {
                    state.udev_events = state.udev_events.saturating_add(1);
                }) {
                Ok(_) => ProbeStatus::available(),
                Err(error) => {
                    ProbeStatus::unavailable(format!("udev calloop registration failed: {error}"))
                }
            }
        }
        Err(error) => {
            ProbeStatus::unavailable(format!("udev construction for seat {seat} failed: {error}"))
        }
    };

    let session_attempt = match session_setup {
        SessionSetup::Attempt => LibSeatSession::new().map_err(libseat_unavailable_reason),
        #[cfg(test)]
        SessionSetup::Unavailable(reason) => Err(reason),
    };
    let (
        session,
        session_seat,
        session_active,
        session_source,
        libinput_source,
        initial_session_active,
    ) = match session_attempt {
        Ok((session, session_notifier)) => {
            let session_seat = session.seat();
            let session_active = session.is_active();
            let session_source = match event_loop.handle().insert_source(
                session_notifier,
                |event, (), state: &mut ProbeLoopState| {
                    state.session_events = state.session_events.saturating_add(1);
                    state.session_active = matches!(event, SessionEvent::ActivateSession);
                },
            ) {
                Ok(_) => ProbeStatus::available(),
                Err(error) => ProbeStatus::unavailable(format!(
                    "libseat calloop registration failed: {error}"
                )),
            };
            let mut libinput =
                Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
            let libinput_source = if libinput.udev_assign_seat(&session_seat).is_err() {
                ProbeStatus::unavailable(format!(
                    "libinput could not assign libseat seat {session_seat}"
                ))
            } else {
                let libinput = LibinputInputBackend::new(libinput);
                match event_loop.handle().insert_source(
                    libinput,
                    |_, (), state: &mut ProbeLoopState| {
                        // Rung E will dispatch these directly through Smithay's seat.
                        // Deliberately no ECS bridge is introduced here.
                        state.ignored_input_events = state.ignored_input_events.saturating_add(1);
                    },
                ) {
                    Ok(_) => ProbeStatus::available(),
                    Err(error) => ProbeStatus::unavailable(format!(
                        "libinput calloop registration failed: {error}"
                    )),
                }
            };
            (
                ProbeStatus::available(),
                Some(session_seat),
                Some(session_active),
                session_source,
                libinput_source,
                session_active,
            )
        }
        Err(reason) => (
            ProbeStatus::unavailable(reason.clone()),
            None,
            None,
            ProbeStatus::unavailable(format!(
                "libseat calloop source unavailable because session acquisition failed: {reason}"
            )),
            ProbeStatus::unavailable(format!(
                "libinput unavailable because session acquisition failed: {reason}"
            )),
            false,
        ),
    };

    Ok((
        event_loop,
        ProbeLoopState {
            session_active: initial_session_active,
            ..ProbeLoopState::default()
        },
        ProtocolProbeFacts {
            session,
            session_seat,
            session_active,
            session_source,
            udev_source,
            libinput_source,
        },
    ))
}

pub(super) fn requested_seat() -> String {
    env::var("XDG_SEAT")
        .ok()
        .filter(|seat| !seat.is_empty())
        .unwrap_or_else(|| "seat0".into())
}

fn discover_drm_cards(seat: &str) -> Result<(Vec<DrmCard>, DrmCard), String> {
    let udev = UdevBackend::new(seat)
        .map_err(|error| format!("udev construction for seat {seat} failed: {error}"))?;
    let mut cards = udev
        .device_list()
        .map(|(device, path)| DrmCard {
            device,
            path: path.to_path_buf(),
        })
        .collect::<Vec<_>>();
    cards.sort_by(|left, right| left.path.cmp(&right.path));
    let primary_path = primary_gpu(seat)
        .map_err(|error| format!("primary-card discovery for seat {seat} failed: {error}"))?
        .ok_or_else(|| format!("udev found no primary DRM card on seat {seat}"))?;
    let primary_card = select_primary_card(&cards, &primary_path)?;
    Ok((cards, primary_card))
}

fn libseat_unavailable_reason(error: impl std::fmt::Debug) -> String {
    format!(
        "libseat session unavailable from this launch context: {error:?}; libseat has logind \
         support, but a process launched in a seatless systemd user-manager session has no seat \
         to acquire, while the live graphical session may already have its sole TakeControl \
         holder; defer seat acquisition to the deliberate Rung D TTY run and do not start seatd"
    )
}

fn select_primary_card(cards: &[DrmCard], primary_path: &Path) -> Result<DrmCard, String> {
    cards
        .iter()
        .find(|card| card.path == primary_path)
        .cloned()
        .ok_or_else(|| {
            format!(
                "udev primary card {} was absent from UdevBackend::device_list",
                primary_path.display()
            )
        })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct KmsProbeReport {
    requested_seat: String,
    card_discovery: ProbeStatus,
    session: ProbeStatus,
    session_seat: Option<String>,
    session_active: Option<bool>,
    cards: Vec<DrmCard>,
    primary_card: Option<DrmCard>,
    session_source: ProbeStatus,
    udev_source: ProbeStatus,
    libinput_source: ProbeStatus,
    protocol_liveness: ProbeStatus,
    vulkan_probe: ProbeStatus,
    enabled_instance_extensions: Vec<String>,
    adapters: Vec<VulkanDrmAdapter>,
    selected_adapter: Option<VulkanDrmAdapter>,
    renderer: ProbeStatus,
    renderer_selected_by_drm: bool,
    atomic_admission: Option<AtomicAdmissionReport>,
    errors: Vec<String>,
}

impl KmsProbeReport {
    pub(crate) fn success(&self) -> bool {
        self.card_discovery.available
            && self.primary_card_is_listed()
            && self.protocol_liveness.available
            && self.vulkan_probe.available
            && self.identity_matches()
            && self.selected_adapter_is_listed()
            && self
                .atomic_admission
                .as_ref()
                .is_some_and(AtomicAdmissionReport::setup_succeeded)
    }

    pub(crate) fn ready_for_bringup(&self) -> bool {
        self.success()
            && self.session.available
            && self.session_source.available
            && self.udev_source.available
            && self.libinput_source.available
            && self.renderer.available
            && self.renderer_selected_by_drm
    }

    fn identity_matches(&self) -> bool {
        let Some(card) = &self.primary_card else {
            return false;
        };
        let Some(adapter) = &self.selected_adapter else {
            return false;
        };
        adapter.primary_device == Some(card.device) || adapter.render_device == Some(card.device)
    }

    fn primary_card_is_listed(&self) -> bool {
        self.primary_card
            .as_ref()
            .is_some_and(|primary| self.cards.contains(primary))
    }

    fn selected_adapter_is_listed(&self) -> bool {
        self.selected_adapter
            .as_ref()
            .is_some_and(|selected| self.adapters.contains(selected))
    }

    pub(crate) fn first_error(&self) -> Option<&str> {
        self.errors
            .first()
            .map(String::as_str)
            .or(self.card_discovery.unavailable_reason.as_deref())
            .or(self.protocol_liveness.unavailable_reason.as_deref())
            .or(self.vulkan_probe.unavailable_reason.as_deref())
            .or_else(|| {
                self.atomic_admission
                    .as_ref()
                    .and_then(|report| report.error.as_deref())
            })
            .or(self.session.unavailable_reason.as_deref())
            .or(self.session_source.unavailable_reason.as_deref())
            .or(self.udev_source.unavailable_reason.as_deref())
            .or(self.libinput_source.unavailable_reason.as_deref())
            .or(self.renderer.unavailable_reason.as_deref())
    }

    pub(crate) fn to_strict_data(&self) -> String {
        let mut out = String::from("{\n");
        push_field(&mut out, 2, "schema_version", "2", true);
        push_string_field(&mut out, 2, "probe", "kms-rung-b", true);
        push_field(
            &mut out,
            2,
            "success",
            if self.success() { "true" } else { "false" },
            true,
        );
        push_field(
            &mut out,
            2,
            "ready_for_bringup",
            bool_text(self.ready_for_bringup()),
            true,
        );
        out.push_str("  \"safety\": {\n");
        push_field(
            &mut out,
            4,
            "drm_device_opened",
            bool_text(
                self.atomic_admission
                    .as_ref()
                    .is_some_and(|report| report.device_opened),
            ),
            true,
        );
        push_field(&mut out, 4, "drm_master_attempted", "false", true);
        push_field(&mut out, 4, "vulkan_surface_created", "false", false);
        out.push_str("  },\n");
        out.push_str("  \"session\": {\n");
        push_field(
            &mut out,
            4,
            "available",
            bool_text(self.session.available),
            true,
        );
        push_optional_string_field(
            &mut out,
            4,
            "unavailable_reason",
            self.session.unavailable_reason.as_deref(),
            true,
        );
        push_string_field(&mut out, 4, "requested_seat", &self.requested_seat, true);
        push_optional_string_field(&mut out, 4, "seat", self.session_seat.as_deref(), true);
        push_optional_bool_field(&mut out, 4, "active", self.session_active, false);
        out.push_str("  },\n");
        out.push_str("  \"udev\": {\n");
        push_field(
            &mut out,
            4,
            "discovery_available",
            bool_text(self.card_discovery.available),
            true,
        );
        push_optional_string_field(
            &mut out,
            4,
            "discovery_unavailable_reason",
            self.card_discovery.unavailable_reason.as_deref(),
            true,
        );
        push_field(
            &mut out,
            4,
            "calloop_registered",
            bool_text(self.udev_source.available),
            true,
        );
        push_optional_string_field(
            &mut out,
            4,
            "calloop_unavailable_reason",
            self.udev_source.unavailable_reason.as_deref(),
            true,
        );
        push_optional_string_field(
            &mut out,
            4,
            "primary_card",
            self.primary_card
                .as_ref()
                .map(|card| card.path.to_string_lossy())
                .as_deref(),
            true,
        );
        push_optional_u64_field(
            &mut out,
            4,
            "primary_card_dev_t",
            self.primary_card.as_ref().map(|card| card.device),
            true,
        );
        out.push_str("    \"cards\": [\n");
        for (index, card) in self.cards.iter().enumerate() {
            let comma = if index + 1 == self.cards.len() {
                ""
            } else {
                ","
            };
            out.push_str("      {\n");
            push_string_field(&mut out, 8, "path", &card.path.to_string_lossy(), true);
            push_field(&mut out, 8, "dev_t", &card.device.to_string(), false);
            out.push_str(&format!("      }}{comma}\n"));
        }
        out.push_str("    ]\n");
        out.push_str("  },\n");
        out.push_str("  \"libinput\": {\n");
        push_field(
            &mut out,
            4,
            "available",
            bool_text(self.libinput_source.available),
            true,
        );
        push_optional_string_field(
            &mut out,
            4,
            "unavailable_reason",
            self.libinput_source.unavailable_reason.as_deref(),
            false,
        );
        out.push_str("  },\n");
        out.push_str("  \"protocol\": {\n");
        push_field(
            &mut out,
            4,
            "libseat_calloop_registered",
            bool_text(self.session_source.available),
            true,
        );
        push_optional_string_field(
            &mut out,
            4,
            "libseat_calloop_unavailable_reason",
            self.session_source.unavailable_reason.as_deref(),
            true,
        );
        push_field(
            &mut out,
            4,
            "live_after_vulkan",
            bool_text(self.protocol_liveness.available),
            true,
        );
        push_optional_string_field(
            &mut out,
            4,
            "liveness_unavailable_reason",
            self.protocol_liveness.unavailable_reason.as_deref(),
            false,
        );
        out.push_str("  },\n");
        out.push_str("  \"vulkan\": {\n");
        push_field(
            &mut out,
            4,
            "probe_completed",
            bool_text(self.vulkan_probe.available),
            true,
        );
        push_optional_string_field(
            &mut out,
            4,
            "probe_unavailable_reason",
            self.vulkan_probe.unavailable_reason.as_deref(),
            true,
        );
        push_field(
            &mut out,
            4,
            "identity_match",
            bool_text(self.identity_matches()),
            true,
        );
        push_field(
            &mut out,
            4,
            "renderer_selected_by_drm",
            bool_text(self.renderer_selected_by_drm),
            true,
        );
        push_field(
            &mut out,
            4,
            "renderer_available",
            bool_text(self.renderer.available),
            true,
        );
        push_optional_string_field(
            &mut out,
            4,
            "renderer_unavailable_reason",
            self.renderer.unavailable_reason.as_deref(),
            true,
        );
        push_string_list(
            &mut out,
            4,
            "enabled_instance_extensions",
            &self.enabled_instance_extensions,
            true,
        );
        out.push_str("    \"adapters\": [\n");
        for (index, adapter) in self.adapters.iter().enumerate() {
            let comma = if index + 1 == self.adapters.len() {
                ""
            } else {
                ","
            };
            out.push_str("      {\n");
            push_string_field(&mut out, 8, "name", &adapter.name, true);
            push_string_field(&mut out, 8, "device_type", &adapter.device_type, true);
            push_optional_u64_field(&mut out, 8, "primary_dev_t", adapter.primary_device, true);
            push_optional_u64_field(&mut out, 8, "render_dev_t", adapter.render_device, false);
            out.push_str(&format!("      }}{comma}\n"));
        }
        out.push_str("    ],\n");
        push_optional_string_field(
            &mut out,
            4,
            "selected_adapter",
            self.selected_adapter
                .as_ref()
                .map(|adapter| adapter.name.as_str()),
            false,
        );
        out.push_str("  },\n");
        push_atomic_admission(&mut out, self.atomic_admission.as_ref());
        push_string_list(&mut out, 2, "errors", &self.errors, false);
        out.push_str("}\n");
        out
    }
}

pub(crate) fn run() -> KmsProbeReport {
    let requested_seat = requested_seat();
    let mut report = KmsProbeReport {
        requested_seat: requested_seat.clone(),
        ..KmsProbeReport::default()
    };
    match discover_drm_cards(&requested_seat) {
        Ok((cards, primary_card)) => {
            report.card_discovery = ProbeStatus::available();
            report.cards = cards;
            report.primary_card = Some(primary_card);
        }
        Err(error) => {
            report.card_discovery = ProbeStatus::unavailable(error.clone());
            report.errors.push(error);
        }
    }

    let protocol = match ProtocolProbeRuntime::start(&requested_seat) {
        Ok(protocol) => {
            report.session.clone_from(&protocol.facts.session);
            report.session_seat.clone_from(&protocol.facts.session_seat);
            report.session_active = protocol.facts.session_active;
            report
                .session_source
                .clone_from(&protocol.facts.session_source);
            report.udev_source.clone_from(&protocol.facts.udev_source);
            report
                .libinput_source
                .clone_from(&protocol.facts.libinput_source);
            Some(protocol)
        }
        Err(error) => {
            report.session = ProbeStatus::unavailable(format!(
                "session prerequisites unavailable because protocol calloop failed: {error}"
            ));
            report.session_source = ProbeStatus::unavailable(format!(
                "libseat calloop source unavailable because protocol calloop failed: {error}"
            ));
            report.udev_source = ProbeStatus::unavailable(format!(
                "udev calloop source unavailable because protocol calloop failed: {error}"
            ));
            report.libinput_source = ProbeStatus::unavailable(format!(
                "libinput calloop source unavailable because protocol calloop failed: {error}"
            ));
            report.protocol_liveness = ProbeStatus::unavailable(error.clone());
            report.errors.push(error);
            None
        }
    };

    let target = report.primary_card.as_ref().map(|card| card.device);
    if let Some(card) = report.primary_card.as_ref() {
        report.atomic_admission = Some(probe_atomic_admission(&card.path, card.device));
    }
    match ManualVulkanRenderer::probe_drm(target) {
        Ok(VulkanDrmProbe {
            enabled_instance_extensions,
            adapters,
            selected_adapter,
        }) => {
            report.vulkan_probe = ProbeStatus::available();
            report
                .enabled_instance_extensions
                .clone_from(&enabled_instance_extensions);
            report.adapters = adapters;
            report.selected_adapter = selected_adapter;
            if let Some(target) = target {
                if report.selected_adapter.is_none() {
                    let reason =
                        format!("no Vulkan physical device has DRM primary/render dev_t {target}");
                    report.renderer = ProbeStatus::unavailable(format!(
                        "DRM-matched renderer was not attempted: {reason}"
                    ));
                    report.errors.push(reason);
                } else {
                    match ManualVulkanRenderer::new_for_drm_offscreen(target) {
                        Ok(renderer) => {
                            report.renderer = ProbeStatus::available();
                            let selected_name = renderer.capabilities().adapter_name.as_str();
                            report.renderer_selected_by_drm = report
                                .selected_adapter
                                .as_ref()
                                .is_some_and(|adapter| adapter.name == selected_name);
                            if !report.renderer_selected_by_drm {
                                report.renderer = ProbeStatus::unavailable(format!(
                                    "DRM selector inspected {} but renderer constructed {}",
                                    report
                                        .selected_adapter
                                        .as_ref()
                                        .map_or("<none>", |adapter| adapter.name.as_str()),
                                    selected_name
                                ));
                            }
                        }
                        Err(error) => {
                            report.renderer = ProbeStatus::unavailable(format!(
                                "DRM-matched Vulkan renderer construction failed: {error}"
                            ));
                        }
                    }
                }
            } else {
                report.renderer = ProbeStatus::unavailable(
                    "DRM-matched renderer was not attempted because card discovery was unavailable",
                );
            }
        }
        Err(error) => {
            let reason = format!("Vulkan DRM inspection failed: {error}");
            report.vulkan_probe = ProbeStatus::unavailable(reason.clone());
            report.renderer = ProbeStatus::unavailable(
                "DRM-matched renderer was not attempted because Vulkan inspection failed",
            );
            report.errors.push(reason);
        }
    }

    if let Some(protocol) = &protocol {
        match protocol.check_liveness() {
            Ok(()) => report.protocol_liveness = ProbeStatus::available(),
            Err(error) => {
                report.protocol_liveness = ProbeStatus::unavailable(error.clone());
                report.errors.push(error);
            }
        }
    }
    report
}

fn push_atomic_admission(out: &mut String, report: Option<&AtomicAdmissionReport>) {
    out.push_str("  \"atomic_admission\": {\n");
    push_field(out, 4, "attempted", bool_text(report.is_some()), true);
    push_optional_string_field(
        out,
        4,
        "device_path",
        report
            .map(|report| report.device_path.to_string_lossy())
            .as_deref(),
        true,
    );
    push_field(
        out,
        4,
        "drm_device_opened",
        bool_text(report.is_some_and(|report| report.device_opened)),
        true,
    );
    push_field(out, 4, "kms_writes_attempted", "false", true);
    push_optional_string_field(
        out,
        4,
        "error",
        report.and_then(|report| report.error.as_deref()),
        true,
    );
    push_field(
        out,
        4,
        "selected_count",
        &report
            .map_or(0, AtomicAdmissionReport::selected_count)
            .to_string(),
        true,
    );
    out.push_str("    \"connectors\": [\n");
    let connectors = report.map_or(&[][..], |report| report.connectors.as_slice());
    for (index, connector) in connectors.iter().enumerate() {
        let comma = if index + 1 == connectors.len() {
            ""
        } else {
            ","
        };
        out.push_str("      {\n");
        push_string_field(out, 8, "name", &connector.connector_name, true);
        push_field(
            out,
            8,
            "connector_id",
            &connector.connector_id.to_string(),
            true,
        );
        match &connector.outcome {
            AtomicAdmissionOutcome::Selected(selected) => {
                let selection = &selected.selection;
                push_string_field(out, 8, "outcome", "selected", true);
                push_field(out, 8, "crtc_id", &selection.crtc_id.to_string(), true);
                push_field(
                    out,
                    8,
                    "primary_plane_id",
                    &selection.primary_plane_id.to_string(),
                    true,
                );
                push_field(out, 8, "format", &selection.format.to_string(), true);
                push_string_field(
                    out,
                    8,
                    "format_name",
                    &super::atomic_present::AtomicFormatModifier {
                        fourcc: selection.format,
                        modifier: selection.modifier,
                    }
                    .format_name(),
                    true,
                );
                push_field(out, 8, "modifier", &selection.modifier.to_string(), true);
                push_string_field(
                    out,
                    8,
                    "modifier_hex",
                    &format!("{:#018x}", selection.modifier),
                    true,
                );
                push_field(
                    out,
                    8,
                    "candidates_total",
                    &selected.total_candidates.to_string(),
                    true,
                );
                push_field(
                    out,
                    8,
                    "candidates_evaluated",
                    &selected.evaluated_candidates.to_string(),
                    true,
                );
                push_field(
                    out,
                    8,
                    "gbm_allocation_attempts",
                    &selected.evaluated_candidates.to_string(),
                    true,
                );
                push_field(
                    out,
                    8,
                    "candidates_not_evaluated",
                    &selected.unevaluated_candidates.to_string(),
                    true,
                );
                match selected.other_admissible_survivors {
                    AtomicSurvivorStatus::None => {
                        push_field(out, 8, "other_admissible_survivors", "false", true);
                        push_string_field(out, 8, "survivor_context", "none", false);
                    }
                    AtomicSurvivorStatus::UnknownNotEvaluated => {
                        push_field(out, 8, "other_admissible_survivors", "null", true);
                        push_string_field(
                            out,
                            8,
                            "survivor_context",
                            "unknown-not-evaluated-by-lazy-admission",
                            false,
                        );
                    }
                }
            }
            AtomicAdmissionOutcome::Rejected(matrix) => {
                push_string_field(out, 8, "outcome", "rejected", true);
                push_optional_string_field(
                    out,
                    8,
                    "route_rejection",
                    matrix.route_rejection.as_deref(),
                    true,
                );
                out.push_str("        \"rejection_matrix\": [\n");
                for (row_index, row) in matrix.candidates.iter().enumerate() {
                    let row_comma = if row_index + 1 == matrix.candidates.len() {
                        ""
                    } else {
                        ","
                    };
                    out.push_str("          {\n");
                    push_field(out, 12, "format", &row.candidate.fourcc.to_string(), true);
                    push_string_field(out, 12, "format_name", &row.candidate.format_name(), true);
                    push_string_field(
                        out,
                        12,
                        "modifier",
                        &format!("{:#018x}", row.candidate.modifier),
                        true,
                    );
                    push_string_field(
                        out,
                        12,
                        "plane_in_formats",
                        &capability_state(&row.plane_in_formats),
                        true,
                    );
                    push_string_field(
                        out,
                        12,
                        "gbm_allocation",
                        &capability_state(&row.gbm_allocation),
                        true,
                    );
                    push_string_field(
                        out,
                        12,
                        "vulkan_colour_attachment",
                        &capability_state(&row.vulkan_colour_attachment),
                        true,
                    );
                    push_string_field(
                        out,
                        12,
                        "wgpu_render_attachment",
                        &capability_state(&row.wgpu_render_attachment),
                        true,
                    );
                    push_string_field(
                        out,
                        12,
                        "selection_policy",
                        &capability_state(&row.selection_policy),
                        false,
                    );
                    out.push_str(&format!("          }}{row_comma}\n"));
                }
                out.push_str("        ]\n");
            }
        }
        out.push_str(&format!("      }}{comma}\n"));
    }
    out.push_str("    ]\n");
    out.push_str("  },\n");
}

fn capability_state(state: &AtomicCapabilityState) -> String {
    match state {
        AtomicCapabilityState::Supported => "supported".into(),
        AtomicCapabilityState::Rejected(reason) => format!("rejected: {reason}"),
    }
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn push_field(out: &mut String, indent: usize, key: &str, value: &str, comma: bool) {
    out.push_str(&" ".repeat(indent));
    out.push_str(&format!(
        "\"{key}\": {value}{}\n",
        if comma { "," } else { "" }
    ));
}

fn push_string_field(out: &mut String, indent: usize, key: &str, value: &str, comma: bool) {
    push_field(
        out,
        indent,
        key,
        &format!("\"{}\"", strict_string(value)),
        comma,
    );
}

fn push_optional_string_field(
    out: &mut String,
    indent: usize,
    key: &str,
    value: Option<&str>,
    comma: bool,
) {
    match value {
        Some(value) => push_string_field(out, indent, key, value, comma),
        None => push_field(out, indent, key, "null", comma),
    }
}

fn push_optional_bool_field(
    out: &mut String,
    indent: usize,
    key: &str,
    value: Option<bool>,
    comma: bool,
) {
    push_field(out, indent, key, value.map_or("null", bool_text), comma);
}

fn push_optional_u64_field(
    out: &mut String,
    indent: usize,
    key: &str,
    value: Option<u64>,
    comma: bool,
) {
    match value {
        Some(value) => push_field(out, indent, key, &value.to_string(), comma),
        None => push_field(out, indent, key, "null", comma),
    }
}

fn push_string_list(out: &mut String, indent: usize, key: &str, values: &[String], comma: bool) {
    let values = values
        .iter()
        .map(|value| format!("\"{}\"", strict_string(value)))
        .collect::<Vec<_>>()
        .join(", ");
    push_field(out, indent, key, &format!("[{values}]"), comma);
}

fn strict_string(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    ' ' | '!'
                        | '#'
                        | '%'
                        | '&'
                        | '('
                        | ')'
                        | '+'
                        | ','
                        | '-'
                        | '.'
                        | '/'
                        | ':'
                        | '<'
                        | '='
                        | '>'
                        | '?'
                        | '@'
                        | '['
                        | ']'
                        | '^'
                        | '_'
                )
            {
                character
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        atomic_present::{
            AtomicCandidateRejection, AtomicConnectorAdmission, AtomicFormatModifier,
            AtomicRejectionMatrix, AtomicSelectedAdmission,
        },
        kms::{AtomicOutputSelection, ConnectorMode},
    };

    type ReportMutation = Box<dyn FnOnce(&mut KmsProbeReport)>;

    fn atomic_setup_report() -> AtomicAdmissionReport {
        AtomicAdmissionReport {
            device_path: "/dev/dri/card0".into(),
            device_opened: true,
            connectors: Vec::new(),
            error: None,
        }
    }

    fn successful_report() -> KmsProbeReport {
        let card = DrmCard {
            device: 200,
            path: "/dev/dri/card0".into(),
        };
        let adapter = VulkanDrmAdapter {
            name: "Integrated GPU".into(),
            device_type: "INTEGRATED_GPU".into(),
            primary_device: Some(200),
            render_device: Some(201),
        };
        KmsProbeReport {
            requested_seat: "seat0".into(),
            card_discovery: ProbeStatus::available(),
            session: ProbeStatus::available(),
            session_seat: Some("seat0".into()),
            session_active: Some(true),
            cards: vec![card.clone()],
            primary_card: Some(card),
            session_source: ProbeStatus::available(),
            udev_source: ProbeStatus::available(),
            libinput_source: ProbeStatus::available(),
            protocol_liveness: ProbeStatus::available(),
            vulkan_probe: ProbeStatus::available(),
            enabled_instance_extensions: vec!["VK_KHR_surface".into()],
            adapters: vec![adapter.clone()],
            selected_adapter: Some(adapter),
            renderer: ProbeStatus::available(),
            renderer_selected_by_drm: true,
            atomic_admission: Some(atomic_setup_report()),
            errors: Vec::new(),
        }
    }

    #[test]
    fn primary_card_must_come_from_the_udev_device_list() {
        let cards = vec![
            DrmCard {
                device: 100,
                path: "/dev/dri/card0".into(),
            },
            DrmCard {
                device: 200,
                path: "/dev/dri/card1".into(),
            },
        ];

        assert_eq!(
            select_primary_card(&cards, Path::new("/dev/dri/card1"))
                .expect("listed primary card")
                .device,
            200
        );
        assert!(
            select_primary_card(&cards, Path::new("/dev/dri/card2"))
                .expect_err("unlisted primary must fail")
                .contains("absent from UdevBackend::device_list")
        );
    }

    #[test]
    fn success_requires_every_rung_b_guard_except_identity() {
        assert!(successful_report().success());

        let mut mutations: Vec<ReportMutation> = vec![
            Box::new(|report| {
                report.card_discovery = ProbeStatus::unavailable("forced discovery failure")
            }),
            Box::new(|report| report.cards.clear()),
            Box::new(|report| {
                report.protocol_liveness = ProbeStatus::unavailable("forced liveness failure")
            }),
            Box::new(|report| {
                report.vulkan_probe = ProbeStatus::unavailable("forced Vulkan failure")
            }),
            Box::new(|report| report.adapters.clear()),
            Box::new(|report| report.atomic_admission = None),
            Box::new(|report| {
                report.atomic_admission.as_mut().unwrap().error =
                    Some("forced atomic setup failure".into())
            }),
        ];
        for mutation in mutations.drain(..) {
            let mut report = successful_report();
            mutation(&mut report);
            assert!(!report.success());
        }
    }

    #[test]
    fn success_rejects_a_fully_populated_hybrid_adapter_identity_mismatch() {
        let mut report = successful_report();
        let adapter = report.selected_adapter.as_mut().expect("fixture adapter");
        adapter.primary_device = Some(300);
        adapter.render_device = Some(301);
        report.adapters = vec![adapter.clone()];

        assert!(report.card_discovery.available);
        assert!(report.primary_card_is_listed());
        assert!(report.protocol_liveness.available);
        assert!(report.vulkan_probe.available);
        assert!(report.selected_adapter_is_listed());
        assert!(!report.success());
    }

    #[test]
    fn success_is_independent_of_seat_bringup_prerequisites() {
        let mut report = successful_report();
        report.session = ProbeStatus::unavailable("forced session failure");
        report.session_source = ProbeStatus::unavailable("forced session source failure");
        report.udev_source = ProbeStatus::unavailable("forced udev source failure");
        report.libinput_source = ProbeStatus::unavailable("forced libinput failure");

        assert!(report.success());
        assert!(!report.ready_for_bringup());
    }

    #[test]
    fn atomic_setup_failure_controls_success_and_first_error_but_connector_rejection_does_not() {
        let mut report = successful_report();
        report.session = ProbeStatus::unavailable("irrelevant seat failure");
        report.atomic_admission.as_mut().unwrap().error =
            Some("forced atomic GBM setup failure".into());
        assert!(!report.success());
        assert_eq!(
            report.first_error(),
            Some("forced atomic GBM setup failure")
        );

        let mut report = successful_report();
        report
            .atomic_admission
            .as_mut()
            .unwrap()
            .connectors
            .push(AtomicConnectorAdmission {
                connector_name: "HDMI-A-1".into(),
                connector_id: 10,
                outcome: AtomicAdmissionOutcome::Rejected(AtomicRejectionMatrix {
                    route_rejection: Some("connector has no route".into()),
                    candidates: Vec::new(),
                }),
            });
        assert!(report.success());
    }

    #[test]
    fn bringup_readiness_requires_rung_b_success_and_every_prerequisite() {
        assert!(successful_report().ready_for_bringup());

        let mut mutations: Vec<ReportMutation> = vec![
            Box::new(|report| {
                report.card_discovery = ProbeStatus::unavailable("forced Rung B failure")
            }),
            Box::new(|report| report.session = ProbeStatus::unavailable("forced session failure")),
            Box::new(|report| {
                report.session_source = ProbeStatus::unavailable("forced session source failure")
            }),
            Box::new(|report| {
                report.udev_source = ProbeStatus::unavailable("forced udev source failure")
            }),
            Box::new(|report| {
                report.libinput_source = ProbeStatus::unavailable("forced libinput failure")
            }),
            Box::new(|report| {
                report.renderer = ProbeStatus::unavailable("forced renderer failure")
            }),
            Box::new(|report| report.renderer_selected_by_drm = false),
        ];
        for mutation in mutations.drain(..) {
            let mut report = successful_report();
            mutation(&mut report);
            assert!(!report.ready_for_bringup());
        }
    }

    #[test]
    fn protocol_liveness_survives_forced_session_unavailability() {
        let protocol = ProtocolProbeRuntime::start_with_session_setup(
            "seat0",
            SessionSetup::Unavailable("forced session failure".into()),
        )
        .expect("session-independent calloop must start");

        assert!(!protocol.facts.session.available);
        assert!(
            protocol
                .facts
                .session
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("forced session failure"))
        );
        protocol
            .check_liveness()
            .expect("control round-trip must not depend on a session");
    }

    #[test]
    fn libseat_reason_names_the_launch_context_and_safe_disposition() {
        let reason = libseat_unavailable_reason("ENOSYS");
        assert!(reason.contains("seatless systemd user-manager session"));
        assert!(reason.contains("libseat has logind support"));
        assert!(reason.contains("Rung D TTY"));
        assert!(reason.contains("do not start seatd"));
    }

    #[test]
    fn strict_data_selected_atomic_form_is_parseable_and_pins_opened_device_safety() {
        let mut report = successful_report();
        report.primary_card.as_mut().expect("fixture card").device = 444;
        report.cards[0].device = 444;
        let adapter = report.selected_adapter.as_mut().expect("fixture adapter");
        adapter.name = "Other GPU".into();
        adapter.primary_device = Some(444);
        report.adapters = vec![adapter.clone()];
        report.atomic_admission.as_mut().unwrap().connectors = vec![AtomicConnectorAdmission {
            connector_name: "HDMI-A-1".into(),
            connector_id: 540,
            outcome: AtomicAdmissionOutcome::Selected(AtomicSelectedAdmission {
                selection: AtomicOutputSelection {
                    connector_id: 540,
                    crtc_id: 151,
                    primary_plane_id: 35,
                    mode: ConnectorMode {
                        width: 1920,
                        height: 1080,
                        refresh_millihz: 60_000,
                        preferred: true,
                        clock_khz: 148_500,
                        hsync: (2008, 2052, 2200),
                        vsync: (1084, 1089, 1125),
                        hskew: 0,
                        vscan: 0,
                        flags: 0,
                    },
                    format: drm_fourcc::DrmFourcc::Xrgb8888 as u32,
                    modifier: 7,
                },
                total_candidates: 4,
                evaluated_candidates: 1,
                unevaluated_candidates: 3,
                other_admissible_survivors: AtomicSurvivorStatus::UnknownNotEvaluated,
            }),
        }];
        let listing = report.to_strict_data();
        assert!(listing.contains("\"schema_version\": 2"));
        assert!(listing.contains("\"probe\": \"kms-rung-b\""));
        assert!(listing.contains("\"success\": true"));
        assert!(listing.contains("\"ready_for_bringup\": true"));
        assert!(listing.contains("\"discovery_available\": true"));
        assert!(listing.contains("\"liveness_unavailable_reason\": null"));
        assert!(listing.contains("\"primary_card_dev_t\": 444"));
        assert!(listing.contains("\"selected_adapter\": \"Other GPU\""));
        assert!(listing.contains("\"drm_master_attempted\": false"));
        assert!(listing.contains("\"drm_device_opened\": true"));
        assert!(listing.contains("\"selected_count\": 1"));
        assert!(listing.contains("\"candidates_evaluated\": 1"));
        assert!(listing.contains("\"other_admissible_survivors\": null"));
        assert_strict_data_parses(&listing);
    }

    #[test]
    fn strict_data_populated_atomic_rejection_matrix_is_parseable() {
        let mut report = successful_report();
        report.atomic_admission.as_mut().unwrap().connectors = vec![AtomicConnectorAdmission {
            connector_name: "HDMI-A-1".into(),
            connector_id: 540,
            outcome: AtomicAdmissionOutcome::Rejected(AtomicRejectionMatrix {
                route_rejection: None,
                candidates: vec![AtomicCandidateRejection {
                    candidate: AtomicFormatModifier {
                        fourcc: drm_fourcc::DrmFourcc::Argb8888 as u32,
                        modifier: 0,
                    },
                    plane_in_formats: AtomicCapabilityState::Supported,
                    gbm_allocation: AtomicCapabilityState::Rejected("allocation refused".into()),
                    vulkan_colour_attachment: AtomicCapabilityState::Rejected(
                        "TRANSFER_SRC rejected".into(),
                    ),
                    wgpu_render_attachment: AtomicCapabilityState::Supported,
                    selection_policy: AtomicCapabilityState::Rejected("opaque formats only".into()),
                }],
            }),
        }];
        let listing = report.to_strict_data();
        assert!(listing.contains("\"selected_count\": 0"));
        assert!(listing.contains("\"rejection_matrix\": ["));
        assert!(listing.contains("\"selection_policy\": \"rejected: opaque formats only\""));
        assert_strict_data_parses(&listing);
    }

    fn assert_strict_data_parses(listing: &str) {
        let status = std::process::Command::new("/opt/cosmix/bin/mix")
            .args([
                "-c",
                "$data = data_parse(env(\"COSMIX_KMS_PROBE_TEST_DATA\")); \
                 if type($data) != \"map\" then die(\"KMS probe report is not a map\") end",
            ])
            .env("COSMIX_KMS_PROBE_TEST_DATA", listing)
            .status()
            .expect("run the authoritative Mix strict-data parser");
        assert!(status.success());
    }

    #[test]
    fn strict_data_replaces_live_string_characters() {
        assert_eq!(strict_string("GPU $bad\\\"line\n"), "GPU ?bad??line?");
    }
}
