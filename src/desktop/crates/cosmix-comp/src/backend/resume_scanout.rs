//! Read-only scanout observations used to decide whether a resume could use a
//! same-mode page flip.  The decision core is deliberately independent of the
//! live KMS feature so the policy remains exhaustively testable offline.

use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) struct ResumeScanoutSnapshot {
    pub(crate) connector: ResumeConnectorState,
    pub(crate) crtc: Option<ResumeCrtcState>,
    pub(crate) primary_plane: Option<ResumePrimaryPlaneState>,
    pub(crate) lifecycle_generation: u64,
    pub(crate) observed_at: Duration,
    pub(crate) old_output_target_existed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) struct ResumeConnectorState {
    pub(crate) id: u32,
    pub(crate) identity: String,
    pub(crate) crtc_id: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) struct ResumeCrtcState {
    pub(crate) id: u32,
    pub(crate) active: bool,
    /// Atomic `MODE_ID` blob identity.  A recreated blob may have a different
    /// ID while containing exactly the same timings, so this is telemetry and
    /// usability evidence rather than part of mode equality.
    pub(crate) mode_blob_id: Option<u64>,
    pub(crate) mode: Option<ResumeModeTiming>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) struct ResumeModeTiming {
    pub(crate) name: String,
    pub(crate) clock_khz: u32,
    pub(crate) size: (u16, u16),
    pub(crate) hsync: (u16, u16, u16),
    pub(crate) vsync: (u16, u16, u16),
    pub(crate) hskew: u16,
    pub(crate) vscan: u16,
    pub(crate) vrefresh: u32,
    pub(crate) mode_type: u32,
    pub(crate) flags: u32,
}

impl ResumeModeTiming {
    #[cfg(any(feature = "kms-live", test))]
    pub(crate) fn refresh_millihz(&self) -> u32 {
        const DRM_MODE_FLAG_INTERLACE: u32 = 1 << 4;
        const DRM_MODE_FLAG_DBLSCAN: u32 = 1 << 5;
        super::scan::exact_refresh_from_timing(
            self.clock_khz,
            self.hsync.2,
            self.vsync.2,
            self.vrefresh,
            self.vscan,
            self.flags & DRM_MODE_FLAG_INTERLACE != 0,
            self.flags & DRM_MODE_FLAG_DBLSCAN != 0,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) struct ResumePlaneGeometry {
    /// Source rectangle values are the kernel's unsigned 16.16 fixed-point
    /// representation. Destination X/Y retain their raw signed bit pattern so
    /// exact equality does not lose negative coordinates.
    pub(crate) src_x: u64,
    pub(crate) src_y: u64,
    pub(crate) src_w: u64,
    pub(crate) src_h: u64,
    pub(crate) dst_x: u64,
    pub(crate) dst_y: u64,
    pub(crate) dst_w: u64,
    pub(crate) dst_h: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) struct ResumeFramebufferFormat {
    pub(crate) size: (u32, u32),
    pub(crate) fourcc: u32,
    pub(crate) modifier: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) struct ResumePlaneFramebuffer {
    pub(crate) fb_id: u32,
    pub(crate) format: ResumeFramebufferFormat,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) struct ResumePrimaryPlaneState {
    pub(crate) id: u32,
    pub(crate) crtc_id: u32,
    pub(crate) geometry: ResumePlaneGeometry,
    pub(crate) fb: Option<ResumePlaneFramebuffer>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) enum ResumePresentationClassification {
    SeamlessPageFlip,
    ModesetRequired(ResumeModesetReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) enum ResumeModesetReason {
    GenerationMismatch,
    InactiveCrtc,
    RouteMismatch,
    ModeMismatch,
    PlaneGeometryOrFormatMismatch,
    NoUsableState,
}

#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) fn classify_resume_scanout(
    before: Option<&ResumeScanoutSnapshot>,
    after: Option<&ResumeScanoutSnapshot>,
) -> ResumePresentationClassification {
    use ResumeModesetReason::{
        GenerationMismatch, InactiveCrtc, ModeMismatch, NoUsableState,
        PlaneGeometryOrFormatMismatch, RouteMismatch,
    };

    let (Some(before), Some(after)) = (before, after) else {
        return ResumePresentationClassification::ModesetRequired(NoUsableState);
    };
    if before.connector.id != after.connector.id
        || before.connector.identity != after.connector.identity
    {
        return ResumePresentationClassification::ModesetRequired(RouteMismatch);
    }

    let before_route_presence = (before.connector.crtc_id.is_some(), before.crtc.is_some());
    let after_route_presence = (after.connector.crtc_id.is_some(), after.crtc.is_some());
    if before_route_presence.0 != before_route_presence.1
        || after_route_presence.0 != after_route_presence.1
        || before_route_presence != after_route_presence
    {
        return ResumePresentationClassification::ModesetRequired(RouteMismatch);
    }
    let (Some(before_crtc), Some(after_crtc)) = (&before.crtc, &after.crtc) else {
        return ResumePresentationClassification::ModesetRequired(NoUsableState);
    };
    if before.connector.crtc_id != Some(before_crtc.id)
        || after.connector.crtc_id != Some(after_crtc.id)
        || before_crtc.id != after_crtc.id
    {
        return ResumePresentationClassification::ModesetRequired(RouteMismatch);
    }

    if !before_crtc.active || !after_crtc.active {
        return ResumePresentationClassification::ModesetRequired(InactiveCrtc);
    }

    match (before.primary_plane.as_ref(), after.primary_plane.as_ref()) {
        (Some(_), None) | (None, Some(_)) => {
            return ResumePresentationClassification::ModesetRequired(RouteMismatch);
        }
        (None, None) => {
            return ResumePresentationClassification::ModesetRequired(NoUsableState);
        }
        (Some(_), Some(_)) => {}
    }
    let before_plane = before.primary_plane.as_ref().expect("both planes exist");
    let after_plane = after.primary_plane.as_ref().expect("both planes exist");
    if before_plane.id != after_plane.id
        || before_plane.crtc_id != before_crtc.id
        || after_plane.crtc_id != after_crtc.id
    {
        return ResumePresentationClassification::ModesetRequired(RouteMismatch);
    }

    let (Some(before_fb), Some(after_fb)) = (&before_plane.fb, &after_plane.fb) else {
        return ResumePresentationClassification::ModesetRequired(NoUsableState);
    };

    let (Some(before_mode), Some(after_mode)) = (&before_crtc.mode, &after_crtc.mode) else {
        return ResumePresentationClassification::ModesetRequired(NoUsableState);
    };
    if before_crtc.mode_blob_id.is_none() || after_crtc.mode_blob_id.is_none() {
        return ResumePresentationClassification::ModesetRequired(NoUsableState);
    }
    if before_mode != after_mode {
        return ResumePresentationClassification::ModesetRequired(ModeMismatch);
    }

    if before_plane.geometry != after_plane.geometry || before_fb.format != after_fb.format {
        return ResumePresentationClassification::ModesetRequired(PlaneGeometryOrFormatMismatch);
    }

    // Generation provenance is the final seamless eligibility gate. Keep it
    // after the more specific route/mode/geometry diagnostics so Slice A's
    // established reason precedence remains stable.
    if !before.old_output_target_existed
        || after.old_output_target_existed
        || after.lifecycle_generation <= before.lifecycle_generation
    {
        return ResumePresentationClassification::ModesetRequired(GenerationMismatch);
    }

    ResumePresentationClassification::SeamlessPageFlip
}

#[cfg(all(feature = "kms-live", not(test)))]
mod drm_capture {
    use std::{
        collections::{BTreeMap, BTreeSet},
        io,
        os::fd::{AsFd, BorrowedFd},
        time::Duration,
    };

    use smithay::reexports::drm::{
        self,
        control::{self, Device as ControlDevice, PlaneType, ResourceHandle, framebuffer, plane},
    };

    use super::{
        ResumeConnectorState, ResumeCrtcState, ResumeFramebufferFormat, ResumeModeTiming,
        ResumePlaneFramebuffer, ResumePlaneGeometry, ResumePrimaryPlaneState,
        ResumeScanoutSnapshot,
    };

    #[derive(Debug)]
    struct BorrowedCard<'fd>(BorrowedFd<'fd>);

    impl AsFd for BorrowedCard<'_> {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0
        }
    }

    impl drm::Device for BorrowedCard<'_> {}
    impl ControlDevice for BorrowedCard<'_> {}

    fn properties<T: ResourceHandle>(
        card: &BorrowedCard<'_>,
        handle: T,
    ) -> io::Result<BTreeMap<String, u64>> {
        card.get_properties(handle)?
            .iter()
            .map(|(property, value)| {
                card.get_property(*property)
                    .map(|info| (info.name().to_string_lossy().into_owned(), *value))
            })
            .collect()
    }

    fn required_property(properties: &BTreeMap<String, u64>, name: &str) -> io::Result<u64> {
        properties.get(name).copied().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("DRM property {name} is absent"),
            )
        })
    }

    fn nonzero(value: u64) -> Option<u32> {
        u32::try_from(value).ok().filter(|value| *value != 0)
    }

    fn mode_timing(mode: control::Mode) -> ResumeModeTiming {
        ResumeModeTiming {
            name: mode.name().to_string_lossy().into_owned(),
            clock_khz: mode.clock(),
            size: mode.size(),
            hsync: mode.hsync(),
            vsync: mode.vsync(),
            hskew: mode.hskew(),
            vscan: mode.vscan(),
            vrefresh: mode.vrefresh(),
            mode_type: mode.mode_type().bits(),
            flags: mode.flags().bits(),
        }
    }

    fn framebuffer_format(
        card: &BorrowedCard<'_>,
        fb_id: u32,
    ) -> io::Result<ResumeFramebufferFormat> {
        let handle = control::from_u32::<framebuffer::Handle>(fb_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "primary plane FB_ID is zero")
        })?;
        // drm-rs 0.14.1 performs GETFB2 and then converts `pixel_format` to a
        // recognised `DrmFourcc` before exposing the returned GEM handles. If
        // that conversion rejects an unknown raw fourcc, those newly allocated
        // handles cannot be closed here. Neither drm-rs nor Smithay re-exports
        // drm-ffi, so eliminating that residual error-path leak requires a
        // future upstream API or an explicitly approved direct dependency.
        let framebuffer = card
            .get_planar_framebuffer(handle)
            .map_err(io::Error::other)?;
        let format = ResumeFramebufferFormat {
            size: framebuffer.size(),
            fourcc: framebuffer.pixel_format() as u32,
            modifier: framebuffer.modifier().map(u64::from),
        };
        let mut unique_handles = BTreeSet::new();
        let mut first_close_error = None;
        for buffer in framebuffer.buffers().into_iter().flatten() {
            if unique_handles.insert(u32::from(buffer))
                && let Err(error) = card.close_buffer(buffer)
                && first_close_error.is_none()
            {
                first_close_error = Some(error);
            }
        }
        if let Some(error) = first_close_error {
            return Err(error);
        }
        Ok(format)
    }

    fn find_attached_primary_plane(
        card: &BorrowedCard<'_>,
        handles: impl IntoIterator<Item = plane::Handle>,
        crtc_id: u32,
    ) -> io::Result<Option<(plane::Handle, BTreeMap<String, u64>)>> {
        for handle in handles {
            let plane_properties = properties(card, handle)?;
            let Some(plane_type) = plane_properties.get("type").copied() else {
                continue;
            };
            let Some(attached_crtc) = plane_properties.get("CRTC_ID").copied().and_then(nonzero)
            else {
                continue;
            };
            if plane_type == PlaneType::Primary as u64 && attached_crtc == crtc_id {
                return Ok(Some((handle, plane_properties)));
            }
        }
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture(
        fd: BorrowedFd<'_>,
        connector_id: u32,
        expected_connector_identity: &str,
        lifecycle_generation: u64,
        observed_at: Duration,
        old_output_target_existed: bool,
        expected_primary_plane_id: Option<u32>,
    ) -> io::Result<ResumeScanoutSnapshot> {
        let card = BorrowedCard(fd);
        // Client capabilities are flags on this DRM file description, not KMS
        // state writes. Enabling both is deliberate: atomic object properties
        // and primary planes are otherwise hidden. The flags persist on the
        // retained libseat fd until that file description is closed.
        drm::Device::set_client_capability(&card, drm::ClientCapability::UniversalPlanes, true)?;
        drm::Device::set_client_capability(&card, drm::ClientCapability::Atomic, true)?;
        let connector_handle = control::from_u32(connector_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "connector object ID is zero")
        })?;
        let connector_info = card.get_connector(connector_handle, false)?;
        let connector_identity = connector_info.to_string();
        if connector_identity != expected_connector_identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "connector {connector_id} resolved as {connector_identity}, expected {expected_connector_identity}"
                ),
            ));
        }
        let connector_properties = properties(&card, connector_handle)?;
        let connector_crtc_id = nonzero(required_property(&connector_properties, "CRTC_ID")?);

        let crtc = connector_crtc_id
            .map(|crtc_id| -> io::Result<ResumeCrtcState> {
                let handle = control::from_u32(crtc_id).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "connector CRTC_ID is zero")
                })?;
                let info = card.get_crtc(handle)?;
                let crtc_properties = properties(&card, handle)?;
                Ok(ResumeCrtcState {
                    id: crtc_id,
                    active: required_property(&crtc_properties, "ACTIVE")? != 0,
                    mode_blob_id: nonzero(required_property(&crtc_properties, "MODE_ID")?)
                        .map(u64::from),
                    mode: info.mode().map(mode_timing),
                })
            })
            .transpose()?;

        let primary_plane = if let Some(crtc_id) = connector_crtc_id {
            let expected_plane = match expected_primary_plane_id.and_then(control::from_u32) {
                Some(handle) => find_attached_primary_plane(&card, [handle], crtc_id)?,
                None => None,
            };
            let attached_plane = match expected_plane {
                Some(plane) => Some(plane),
                None => find_attached_primary_plane(&card, card.plane_handles()?, crtc_id)?,
            };
            attached_plane
                .map(
                    |(handle, plane_properties)| -> io::Result<ResumePrimaryPlaneState> {
                        let fb = nonzero(required_property(&plane_properties, "FB_ID")?)
                            .map(|fb_id| {
                                framebuffer_format(&card, fb_id)
                                    .map(|format| ResumePlaneFramebuffer { fb_id, format })
                            })
                            .transpose()?;
                        Ok(ResumePrimaryPlaneState {
                            id: handle.into(),
                            crtc_id,
                            geometry: ResumePlaneGeometry {
                                src_x: required_property(&plane_properties, "SRC_X")?,
                                src_y: required_property(&plane_properties, "SRC_Y")?,
                                src_w: required_property(&plane_properties, "SRC_W")?,
                                src_h: required_property(&plane_properties, "SRC_H")?,
                                dst_x: required_property(&plane_properties, "CRTC_X")?,
                                dst_y: required_property(&plane_properties, "CRTC_Y")?,
                                dst_w: required_property(&plane_properties, "CRTC_W")?,
                                dst_h: required_property(&plane_properties, "CRTC_H")?,
                            },
                            fb,
                        })
                    },
                )
                .transpose()?
        } else {
            None
        };

        Ok(ResumeScanoutSnapshot {
            connector: ResumeConnectorState {
                id: connector_id,
                identity: connector_identity,
                crtc_id: connector_crtc_id,
            },
            crtc,
            primary_plane,
            lifecycle_generation,
            observed_at,
            old_output_target_existed,
        })
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
pub(crate) use drm_capture::capture;

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ResumeScanoutSnapshot {
        ResumeScanoutSnapshot {
            connector: ResumeConnectorState {
                id: 72,
                identity: "DP-1".into(),
                crtc_id: Some(42),
            },
            crtc: Some(ResumeCrtcState {
                id: 42,
                active: true,
                mode_blob_id: Some(301),
                mode: Some(ResumeModeTiming {
                    name: "3840x2160".into(),
                    clock_khz: 594_000,
                    size: (3840, 2160),
                    hsync: (4016, 4104, 4400),
                    vsync: (2168, 2178, 2250),
                    hskew: 0,
                    vscan: 0,
                    vrefresh: 60,
                    mode_type: 0,
                    flags: 5,
                }),
            }),
            primary_plane: Some(ResumePrimaryPlaneState {
                id: 31,
                crtc_id: 42,
                geometry: ResumePlaneGeometry {
                    src_x: 0,
                    src_y: 0,
                    src_w: 3840 << 16,
                    src_h: 2160 << 16,
                    dst_x: 0,
                    dst_y: 0,
                    dst_w: 3840,
                    dst_h: 2160,
                },
                fb: Some(ResumePlaneFramebuffer {
                    fb_id: 90,
                    format: ResumeFramebufferFormat {
                        size: (3840, 2160),
                        fourcc: 875_713_112,
                        modifier: Some(0),
                    },
                }),
            }),
            lifecycle_generation: 1,
            observed_at: Duration::from_secs(10),
            old_output_target_existed: true,
        }
    }

    fn classify_after(
        mutate: impl FnOnce(&mut ResumeScanoutSnapshot),
    ) -> ResumePresentationClassification {
        let before = snapshot();
        let mut after = before.clone();
        after.lifecycle_generation = 2;
        after.observed_at += Duration::from_secs(1);
        after.old_output_target_existed = false;
        after
            .primary_plane
            .as_mut()
            .unwrap()
            .fb
            .as_mut()
            .unwrap()
            .fb_id += 1;
        mutate(&mut after);
        classify_resume_scanout(Some(&before), Some(&after))
    }

    #[test]
    fn resume_scanout_classifier_allows_only_primary_fb_replacement() {
        assert_eq!(
            classify_after(|_| {}),
            ResumePresentationClassification::SeamlessPageFlip
        );
    }

    #[test]
    fn resume_mode_diagnostic_refresh_uses_the_scan_selection_calculation() {
        let mode = snapshot().crtc.unwrap().mode.unwrap();
        assert_eq!(mode.refresh_millihz(), 60_000);
    }

    #[test]
    fn resume_scanout_classifier_rejects_stale_or_inverted_generations() {
        assert_eq!(
            classify_after(|after| after.lifecycle_generation = 1),
            ResumePresentationClassification::ModesetRequired(
                ResumeModesetReason::GenerationMismatch
            )
        );
        assert_eq!(
            classify_after(|after| after.old_output_target_existed = true),
            ResumePresentationClassification::ModesetRequired(
                ResumeModesetReason::GenerationMismatch
            )
        );
        let mut before = snapshot();
        before.old_output_target_existed = false;
        let mut after = before.clone();
        after.lifecycle_generation += 1;
        assert_eq!(
            classify_resume_scanout(Some(&before), Some(&after)),
            ResumePresentationClassification::ModesetRequired(
                ResumeModesetReason::GenerationMismatch
            )
        );
    }

    #[test]
    fn recycled_numeric_ids_do_not_override_connector_identity_mismatch() {
        assert_eq!(
            classify_after(|after| {
                // Every numeric KMS object ID is deliberately left unchanged,
                // modelling an ID recycled for a different physical route.
                after.connector.identity = "DP-9".into();
            }),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::RouteMismatch)
        );
    }

    #[test]
    fn resume_scanout_classifier_allows_same_fb_and_different_mode_blob_identity() {
        assert_eq!(
            classify_after(|after| {
                after
                    .primary_plane
                    .as_mut()
                    .unwrap()
                    .fb
                    .as_mut()
                    .unwrap()
                    .fb_id = 90;
                after.crtc.as_mut().unwrap().mode_blob_id = Some(999);
            }),
            ResumePresentationClassification::SeamlessPageFlip
        );
    }

    #[test]
    fn resume_scanout_classifier_requires_usable_before_and_after_state() {
        let before = snapshot();
        assert_eq!(
            classify_resume_scanout(None, Some(&before)),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::NoUsableState)
        );
        assert_eq!(
            classify_resume_scanout(Some(&before), None),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::NoUsableState)
        );
        assert_eq!(
            classify_after(|after| after.crtc.as_mut().unwrap().mode = None),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::NoUsableState)
        );
        assert_eq!(
            classify_after(|after| after.crtc.as_mut().unwrap().mode_blob_id = None),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::NoUsableState)
        );
    }

    #[test]
    fn resume_scanout_classifier_connector_identity_precedes_missing_state() {
        let before = snapshot();
        let mut after = before.clone();
        after.connector.identity = "DP-2".into();
        after.crtc = None;
        after.primary_plane = None;
        assert_eq!(
            classify_resume_scanout(Some(&before), Some(&after)),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::RouteMismatch)
        );
    }

    #[test]
    fn resume_scanout_classifier_distinguishes_partial_different_and_absent_routes() {
        assert_eq!(
            classify_after(|after| {
                after.connector.crtc_id = None;
                after.crtc = None;
                after.primary_plane = None;
            }),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::RouteMismatch)
        );
        let mut before = snapshot();
        let mut after = before.clone();
        before.connector.crtc_id = None;
        before.crtc = None;
        before.primary_plane = None;
        after.connector.crtc_id = None;
        after.crtc = None;
        after.primary_plane = None;
        assert_eq!(
            classify_resume_scanout(Some(&before), Some(&after)),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::NoUsableState)
        );
        assert_eq!(
            classify_after(|after| after.crtc = None),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::RouteMismatch)
        );
    }

    #[test]
    fn resume_scanout_classifier_reports_inactive_before_after_and_both_crtcs() {
        let mut before = snapshot();
        let after = before.clone();
        before.crtc.as_mut().unwrap().active = false;
        assert_eq!(
            classify_resume_scanout(Some(&before), Some(&after)),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::InactiveCrtc)
        );
        assert_eq!(
            classify_after(|after| after.crtc.as_mut().unwrap().active = false),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::InactiveCrtc)
        );
        let mut before = snapshot();
        let mut after = before.clone();
        before.crtc.as_mut().unwrap().active = false;
        after.crtc.as_mut().unwrap().active = false;
        assert_eq!(
            classify_resume_scanout(Some(&before), Some(&after)),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::InactiveCrtc)
        );
    }

    #[test]
    fn resume_scanout_classifier_distinguishes_one_and_two_missing_primary_planes() {
        assert_eq!(
            classify_after(|after| after.primary_plane = None),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::RouteMismatch)
        );
        let mut before = snapshot();
        let mut after = before.clone();
        before.primary_plane = None;
        after.primary_plane = None;
        assert_eq!(
            classify_resume_scanout(Some(&before), Some(&after)),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::NoUsableState)
        );
    }

    #[test]
    fn resume_scanout_classifier_reports_every_routed_object_mismatch_boundary() {
        let mutations: [fn(&mut ResumeScanoutSnapshot); 6] = [
            |after| after.connector.id += 1,
            |after| after.connector.identity = "DP-2".into(),
            |after| after.connector.crtc_id = Some(43),
            |after| after.crtc.as_mut().unwrap().id = 43,
            |after| after.primary_plane.as_mut().unwrap().id += 1,
            |after| after.primary_plane.as_mut().unwrap().crtc_id += 1,
        ];
        for mutate in mutations {
            assert_eq!(
                classify_after(mutate),
                ResumePresentationClassification::ModesetRequired(
                    ResumeModesetReason::RouteMismatch
                )
            );
        }
    }

    #[test]
    fn resume_scanout_classifier_isolates_before_connector_to_crtc_inconsistency() {
        let mut before = snapshot();
        let after = before.clone();
        before.connector.crtc_id = Some(43);
        assert_eq!(
            classify_resume_scanout(Some(&before), Some(&after)),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::RouteMismatch)
        );
    }

    #[test]
    fn resume_scanout_classifier_isolates_before_plane_to_crtc_inconsistency() {
        let mut before = snapshot();
        let after = before.clone();
        before.primary_plane.as_mut().unwrap().crtc_id = 43;
        assert_eq!(
            classify_resume_scanout(Some(&before), Some(&after)),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::RouteMismatch)
        );
    }

    #[test]
    fn resume_scanout_classifier_isolates_cross_snapshot_crtc_change() {
        let before = snapshot();
        let mut after = before.clone();
        after.connector.crtc_id = Some(43);
        after.crtc.as_mut().unwrap().id = 43;
        after.primary_plane.as_mut().unwrap().crtc_id = 43;
        assert_eq!(
            classify_resume_scanout(Some(&before), Some(&after)),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::RouteMismatch)
        );
    }

    #[test]
    fn resume_scanout_classifier_requires_a_framebuffer_on_both_primary_planes() {
        assert_eq!(
            classify_after(|after| after.primary_plane.as_mut().unwrap().fb = None),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::NoUsableState)
        );
        let mut before = snapshot();
        let mut after = before.clone();
        before.primary_plane.as_mut().unwrap().fb = None;
        after.primary_plane.as_mut().unwrap().fb = None;
        assert_eq!(
            classify_resume_scanout(Some(&before), Some(&after)),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::NoUsableState)
        );
    }

    #[test]
    fn resume_scanout_classifier_reports_mode_timing_mismatch() {
        assert_eq!(
            classify_after(|after| {
                after
                    .crtc
                    .as_mut()
                    .unwrap()
                    .mode
                    .as_mut()
                    .unwrap()
                    .clock_khz += 1;
            }),
            ResumePresentationClassification::ModesetRequired(ResumeModesetReason::ModeMismatch)
        );
    }

    #[test]
    fn resume_scanout_classifier_reports_plane_geometry_and_format_mismatches() {
        assert_eq!(
            classify_after(|after| after.primary_plane.as_mut().unwrap().geometry.dst_w -= 1),
            ResumePresentationClassification::ModesetRequired(
                ResumeModesetReason::PlaneGeometryOrFormatMismatch
            )
        );
        assert_eq!(
            classify_after(|after| {
                after
                    .primary_plane
                    .as_mut()
                    .unwrap()
                    .fb
                    .as_mut()
                    .unwrap()
                    .format
                    .fourcc += 1;
            }),
            ResumePresentationClassification::ModesetRequired(
                ResumeModesetReason::PlaneGeometryOrFormatMismatch
            )
        );
        assert_eq!(
            classify_after(|after| {
                after
                    .primary_plane
                    .as_mut()
                    .unwrap()
                    .fb
                    .as_mut()
                    .unwrap()
                    .format
                    .modifier = None;
            }),
            ResumePresentationClassification::ModesetRequired(
                ResumeModesetReason::PlaneGeometryOrFormatMismatch
            )
        );
    }
}
