//! The Controller surface the windowed chrome (E1f `app.rs`) drives, agreed
//! with E1f and approved by the lead on 2026-09-26 as additive to the Stage S
//! freeze: typed action args (find / replace / save-as / goto / close), the
//! infobar's choices (keep mine, take the service's, keep as new, dismiss a
//! conflict), recovered buffers, `ced.layout` / `ced.stats` feeds, and lint
//! results mapped through the delta log (plan §4.10).
//!
//! Every method ends like the frozen ones: pending `ced.wait` waiters are
//! re-evaluated and the effects are returned for the host to perform.

use std::collections::VecDeque;
use std::path::PathBuf;

use cosmix_edit_client::highlight::ResultTag;
use cosmix_edit_client::mirror::{Mirror, Phase, ServerOp};
use cosmix_edit_client::types::{Intent, Level, Notice, Outgoing, TabId, ViewDelta};
use cosmix_edit_core::anchor::Selection;
use cosmix_edit_core::limits::{FIND_MAX_LIMIT, MAX_OPS_PER_TXN, MAX_REQUEST_TEXT_BYTES};
use cosmix_edit_core::wire;
use serde_json::{Value, json};

use super::{Controller, DEADLINE_MS, Effect, Req, code, reply_result};
use crate::actions::ActionId;
use crate::verbs;

/// A decision only a human can make; the chrome shows a dialog and answers
/// through [`Controller::on_action_args`] (or the methods below).
#[derive(Debug, Clone, PartialEq)]
pub enum Prompt {
    /// `edit.close` refused `CONFLICT dirty`: this is the last holder of
    /// unsaved text. Answer `file.close {save:true}` or `{force:true}`.
    CloseDirty { tab: TabId, intent: Intent },
    /// A save refused `disk_modified`. Answer `file.save {force:true}`.
    DiskModified { tab: TabId, intent: Intent },
    /// Recovered buffers no tab holds (once, after start). Answer
    /// [`Controller::open_recovered`] / [`Controller::discard_recovered`].
    Recovered { buffers: Vec<RecoveredRow> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredRow {
    pub buffer: String,
    pub path: Option<String>,
    pub name: String,
    pub bytes: Option<usize>,
}

/// Samples kept for the `ced.stats` percentiles.
const FRAME_SAMPLES: usize = 4096;
/// A lint capture stops retaining deltas past this many (its result is then
/// dropped as stale instead of mapped).
const LINT_DELTAS_MAX: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FindKind {
    Next,
    Prev,
    Replace,
    ReplaceAll,
}

/// One find / replace request, carried through its async steps.
#[derive(Debug, Clone)]
pub(super) struct FindJob {
    kind: FindKind,
    pattern: String,
    regex: bool,
    case: bool,
    replacement: String,
    expand: bool,
    intent: Intent,
    /// Searched past the end (Next) / start (Prev) once already.
    wrapped: bool,
    /// Replace: the selection check is done; now find the next match.
    checked: bool,
    /// Prev / ReplaceAll: matches collected across pages, and their rev.
    found: Vec<wire::MatchW>,
    rev: Option<u64>,
    /// ReplaceAll: re-found once after the rev moved between pages.
    refound: bool,
}

/// A lint run's captured input and the deltas since (plan §4.10).
pub(super) struct LintCapture {
    tag: ResultTag,
    text: String,
    deltas: Vec<ViewDelta>,
    overflow: bool,
}

impl LintCapture {
    pub(super) fn record(&mut self, d: &ViewDelta) {
        if self.deltas.len() >= LINT_DELTAS_MAX {
            self.overflow = true;
            self.deltas.clear();
        } else if !self.overflow {
            self.deltas.push(d.clone());
        }
    }
}

#[derive(Default)]
pub(super) struct Frames {
    pub(super) count: u64,
    pub(super) view_us: VecDeque<u64>,
    pub(super) next_frame_us: VecDeque<u64>,
}

fn push_sample(q: &mut VecDeque<u64>, v: u64) {
    if q.len() >= FRAME_SAMPLES {
        q.pop_front();
    }
    q.push_back(v);
}

pub(super) fn percentiles(q: &VecDeque<u64>) -> verbs::Percentiles {
    if q.is_empty() {
        return verbs::Percentiles::default();
    }
    let mut v: Vec<u64> = q.iter().copied().collect();
    v.sort_unstable();
    let at = |p: usize| v[((v.len() - 1) * p) / 100];
    verbs::Percentiles { p50: at(50), p95: at(95), p99: at(99), max: *v.last().unwrap_or(&0) }
}

/// `$0`…`$9` and `${n}` from the match text and its groups; `$$` is `$`.
/// `None` when a referenced group was truncated or omitted by editd.
pub(super) fn expand_replacement(template: &str, m: &wire::MatchW) -> Option<String> {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let n = match chars.peek() {
            Some('$') => {
                chars.next();
                out.push('$');
                continue;
            }
            Some('{') => {
                chars.next();
                let digits: String = chars.by_ref().take_while(|c| *c != '}').collect();
                digits.parse::<usize>().ok()?
            }
            Some(d) if d.is_ascii_digit() => {
                let n = d.to_digit(10)? as usize;
                chars.next();
                n
            }
            _ => {
                out.push('$');
                continue;
            }
        };
        if n == 0 {
            if m.text_truncated {
                return None;
            }
            out.push_str(&m.text);
            continue;
        }
        let groups = m.groups.as_ref()?;
        match groups.get(n - 1) {
            Some(Some(g)) => out.push_str(g),
            Some(None) => {}
            None if m.groups_truncated => return None,
            None => {}
        }
    }
    Some(out)
}

impl Controller {
    /// The same path `ced.action {id, args}` takes, so the window and the Bus
    /// behave identically. Args by action:
    /// - `file.open {paths:[S]}` (refused without paths);
    /// - `file.save {force?}`; `file.save_as {path, force?}`;
    /// - `file.close {force?}` (discard) | `{save:true}` (save, then close);
    /// - `search.find_next` / `search.find_prev {pattern, regex?, case?}`:
    ///   waits for idle, searches from the selection with one wrap, selects the
    ///   match, posts "Wrapped" / "No matches";
    /// - `search.replace {pattern, replacement, regex?, case?, expand?}`:
    ///   replaces the selection if it is a match (`ApplyAt` with `expect_rev`),
    ///   then finds the next; `search.replace_all` (same args) preflights at
    ///   most 10,000 matches and 1 MiB inserted, then one `ApplyAt`;
    ///   `expand` expands `$n` from `groups:true`;
    /// - `search.goto_line {line, col?}`.
    pub fn on_action_args(&mut self, tab: Option<TabId>, action: ActionId, args: Option<Value>, intent: Intent) -> Vec<Effect> {
        let mut fx = Vec::new();
        if let Err((_, msg)) = self.action_args(tab, action, args.as_ref(), intent, &mut fx) {
            fx.push(Effect::Notice { tab, notice: Notice::Message { level: Level::Warn, text: msg } });
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// Make `tab` active (the tab strip); clears its "edited by others" mark.
    pub fn select_tab(&mut self, tab: TabId) -> Vec<Effect> {
        let mut fx = Vec::new();
        if self.tab(tab).is_some() {
            self.active = Some(tab);
            if let Some(x) = self.x.get_mut(&tab) {
                x.agent_since_focus = false;
            }
            self.session_changed(&mut fx);
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// Dismiss the conflict recorded for remote rev `rev` (infobar ×).
    pub fn dismiss_conflict(&mut self, tab: TabId, rev: u64) {
        if let Some(m) = self.tab_mut(tab).and_then(|t| t.mirror.as_mut()) {
            m.dismiss_conflict(rev);
        }
    }

    /// "Keep mine" after a reattach that differed (plan §3.8).
    pub fn keep_mine(&mut self, tab: TabId, intent: Intent) -> Vec<Effect> {
        let mut fx = Vec::new();
        if let Some(i) = self.tabs.iter().position(|t| t.id == tab)
            && let Some(m) = self.tabs[i].mirror.as_mut()
        {
            let step = m.keep_mine(intent, &mut self.ids);
            self.drive(tab, step, &mut fx);
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// "Take the service's": drop the detached copy.
    pub fn take_service(&mut self, tab: TabId) -> Vec<Effect> {
        let mut fx = Vec::new();
        if let Some(step) = self.tab_mut(tab).and_then(|t| t.mirror.as_mut()).map(Mirror::take_theirs) {
            self.drive(tab, step, &mut fx);
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// "Keep as new buffer" / "Save mine as…": the tab's own text (its
    /// detached copy when it has one, else its view) goes into a new scratch
    /// buffer in a new, active tab, inserted by `intent` once that buffer is
    /// live (a >1 MiB text goes as consecutive ≤1 MiB inserts).
    pub fn keep_as_new(&mut self, tab: TabId, intent: Intent) -> Vec<Effect> {
        let mut fx = Vec::new();
        let text = self.tab(tab).and_then(|t| t.mirror.as_ref()).map(|m| match m.detached_copy() {
            Some(d) => d.text.clone(),
            None => super::text_string(m.text()),
        });
        if let Some(text) = text {
            let id = self.new_tab(None);
            self.send_open(id, None, false, &mut fx);
            self.active = Some(id);
            if let Some(x) = self.x.get_mut(&id) {
                x.fill = Some((text, Intent { tab: id, ..intent }));
            }
            self.session_changed(&mut fx);
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// Open a recovered buffer offered by [`Prompt::Recovered`] in a new tab.
    pub fn open_recovered(&mut self, buffer: &str, intent: Intent) -> Vec<Effect> {
        let _ = intent;
        let mut fx = Vec::new();
        if let Some(b) = self.recovered.iter().find(|b| b.buffer == buffer).cloned() {
            match &b.path {
                Some(p) => {
                    self.open_many(std::slice::from_ref(p), None, &mut fx);
                }
                None => {
                    let id = self.new_tab(None);
                    let open = super::open_reply_of(&self.recovered_epoch, &b);
                    self.active = Some(id);
                    self.attach(id, false, &open, &mut fx);
                }
            }
            self.recovered.retain(|r| r.buffer != buffer);
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// Discard a recovered buffer: `edit.close {force:true}` (its unsaved text
    /// and recovery files are gone).
    pub fn discard_recovered(&mut self, buffer: &str) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.recovered.retain(|r| r.buffer != buffer);
        let out = Outgoing {
            verb: "edit.close".into(),
            body: json!({"buffer": buffer, "force": true, "origin": cosmix_edit_client::types::UI_ORIGIN}).to_string(),
            op_id: None,
            deadline_ms: DEADLINE_MS,
        };
        self.send(out, Req::Discard, &mut fx);
        fx
    }

    /// The last frame's engine geometry; `ced.layout` answers from it (none,
    /// or headless → UNAVAILABLE).
    pub fn set_layout(&mut self, layout: Option<verbs::LayoutReply>) {
        self.layout = layout;
    }

    /// One frame's timings for `ced.stats` (`view_us`; `next_frame_us` when
    /// the frame carried a key's gen).
    pub fn record_frame(&mut self, view_us: u64, next_frame_us: Option<u64>) {
        self.frames.count += 1;
        push_sample(&mut self.frames.view_us, view_us);
        if let Some(n) = next_frame_us {
            push_sample(&mut self.frames.next_frame_us, n);
        }
    }

    /// Capture the tab's text for a lint run (plan §4.10): the tag at the
    /// current gen, the text, and the file's directory (the lint's cwd). From
    /// here the tab retains its deltas so [`Controller::on_lint`] can map (or
    /// drop) the result. `None` unless the tab is live.
    pub fn lint_capture(&mut self, tab: TabId, cfg: u64) -> Option<(ResultTag, String, Option<PathBuf>)> {
        let t = self.tab(tab)?;
        let m = t.mirror.as_ref().filter(|m| matches!(m.phase(), Phase::Live))?;
        let tag = ResultTag {
            epoch: m.epoch().to_string(),
            buffer: m.buffer().to_string(),
            view_gen: m.view_gen(),
            language: m.meta().language.clone(),
            cfg,
        };
        let text = super::text_string(m.text());
        let cwd = m.meta().path.as_deref().and_then(|p| std::path::Path::new(p).parent()).map(PathBuf::from);
        if let Some(x) = self.x.get_mut(&tab) {
            x.lint = Some(LintCapture { tag: tag.clone(), text: text.clone(), deltas: Vec::new(), overflow: false });
        }
        Some((tag, text, cwd))
    }

    /// A lint result for a [`Controller::lint_capture`]: `Ok(json)` is handed
    /// to `Diagnostics::accept` with the deltas since the capture; a result
    /// for a superseded capture, another buffer or epoch is dropped;
    /// `Err(message)` is posted as a notice.
    pub fn on_lint(&mut self, tab: TabId, tag: ResultTag, result: Result<String, String>) -> Vec<Effect> {
        let mut fx = Vec::new();
        let capture = self.x.get_mut(&tab).and_then(|x| x.lint.take_if(|c| c.tag == tag));
        match (capture, result) {
            (Some(c), Ok(json)) if !c.overflow => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab)
                    && let Some(m) = t.mirror.as_ref()
                {
                    let current = ResultTag { view_gen: m.view_gen(), ..tag.clone() };
                    if let Err(e) = t.diagnostics.accept(&current, tag, &c.text, &json, &c.deltas) {
                        fx.push(Effect::Notice {
                            tab: Some(tab),
                            notice: Notice::Message { level: Level::Warn, text: format!("lint result not used: {e:?}") },
                        });
                    }
                }
            }
            (Some(_), Err(msg)) => {
                fx.push(Effect::Notice { tab: Some(tab), notice: Notice::Message { level: Level::Warn, text: msg } })
            }
            _ => {}
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// The latest `edit.info` (its `volatile` drives the UNPROTECTED badge).
    pub fn edit_info(&self) -> Option<&verbs::EditInfo> {
        self.edit_info.epoch.as_ref().map(|_| &self.edit_info)
    }

    // ── internals ────────────────────────────────────────────────────────────

    /// [`Controller::action`] plus the args-taking actions.
    pub(super) fn action_args(
        &mut self,
        tab: Option<TabId>,
        action: ActionId,
        args: Option<&Value>,
        intent: Intent,
        fx: &mut Vec<Effect>,
    ) -> Result<Option<Value>, (&'static str, String)> {
        let tab = tab.or(self.active);
        let arg_str = |k: &str| args.and_then(|a| a.get(k)).and_then(Value::as_str).map(str::to_string);
        let arg_bool = |k: &str| args.and_then(|a| a.get(k)).and_then(Value::as_bool).unwrap_or(false);
        let need = || tab.ok_or((code::NOT_FOUND, "no tab is open".to_string()));
        match action {
            ActionId::FileOpen => {
                let paths: Vec<String> = args
                    .and_then(|a| a.get("paths"))
                    .and_then(|p| serde_json::from_value(p.clone()).ok())
                    .filter(|p: &Vec<String>| !p.is_empty())
                    .ok_or((code::INVALID_ARGUMENT, "file.open needs {paths:[…]}".to_string()))?;
                let ids = self.open_many(&paths, None, fx);
                Ok(Some(json!({"tabs": ids})))
            }
            ActionId::FileSave | ActionId::FileSaveAs => {
                let t = need()?;
                let path = arg_str("path");
                if action == ActionId::FileSaveAs && path.is_none() {
                    return Err((code::INVALID_ARGUMENT, "file.save_as needs {path}".into()));
                }
                if let Some(x) = self.x.get_mut(&t) {
                    x.save_intent = Some(intent.clone());
                }
                self.server_op(t, ServerOp::Save { path, force: arg_bool("force") }, intent, fx).map_err(|e| (code::CONFLICT, e))?;
                Ok(None)
            }
            ActionId::FileClose => {
                let t = need()?;
                if arg_bool("save") {
                    if let Some(x) = self.x.get_mut(&t) {
                        x.close_after_save = Some(intent.clone());
                        x.save_intent = Some(intent.clone());
                    }
                    self.server_op(t, ServerOp::Save { path: None, force: false }, intent, fx).map_err(|e| (code::CONFLICT, e))?;
                } else {
                    self.close(t, arg_bool("force"), None, intent, fx);
                }
                Ok(None)
            }
            ActionId::SearchFindNext | ActionId::SearchFindPrev | ActionId::SearchReplace | ActionId::SearchReplaceAll => {
                let t = need()?;
                let pattern = arg_str("pattern").filter(|p| !p.is_empty()).ok_or((code::INVALID_ARGUMENT, format!("{} needs {{pattern}}", action.id())))?;
                let kind = match action {
                    ActionId::SearchFindNext => FindKind::Next,
                    ActionId::SearchFindPrev => FindKind::Prev,
                    ActionId::SearchReplace => FindKind::Replace,
                    _ => FindKind::ReplaceAll,
                };
                let replacement = arg_str("replacement").unwrap_or_default();
                if matches!(kind, FindKind::Replace | FindKind::ReplaceAll) && args.and_then(|a| a.get("replacement")).is_none() {
                    return Err((code::INVALID_ARGUMENT, format!("{} needs {{replacement}}", action.id())));
                }
                let job = FindJob {
                    kind,
                    pattern,
                    regex: arg_bool("regex"),
                    case: args.and_then(|a| a.get("case")).and_then(Value::as_bool).unwrap_or(true),
                    replacement,
                    expand: arg_bool("expand"),
                    intent,
                    wrapped: false,
                    checked: false,
                    found: Vec::new(),
                    rev: None,
                    refound: false,
                };
                self.start_find(t, job, fx);
                Ok(None)
            }
            ActionId::SearchGotoLine => {
                let t = need()?;
                let line = args.and_then(|a| a.get("line")).and_then(Value::as_u64).ok_or((code::INVALID_ARGUMENT, "search.goto_line needs {line}".to_string()))?;
                let col = args.and_then(|a| a.get("col")).and_then(Value::as_u64).unwrap_or(1);
                self.goto(t, (line as usize, col as usize));
                self.caret_moved(t, fx);
                Ok(None)
            }
            ActionId::ViewClearMarkers => {
                if let Some(t) = tab.and_then(|t| self.tab_mut(t)) {
                    t.editor.markers.changed.clear();
                }
                if let Some(x) = tab.and_then(|t| self.x.get_mut(&t)) {
                    x.agent_since_focus = false;
                }
                Ok(None)
            }
            _ => self.action(tab, action, intent, fx),
        }
    }

    /// Run `job` on `tab` now if its pipeline is idle, else once it drains
    /// (offsets from `edit.find` must match the view).
    pub(super) fn start_find(&mut self, tab: TabId, job: FindJob, fx: &mut Vec<Effect>) {
        let idle = self.tab(tab).and_then(|t| t.mirror.as_ref()).is_some_and(Mirror::is_idle);
        if !idle {
            if let Some(x) = self.x.get_mut(&tab) {
                x.find = Some(job);
            }
            return;
        }
        let Some(t) = self.tab(tab) else { return };
        let Some(m) = t.mirror.as_ref() else { return };
        let sel = t.editor.sel;
        let (lo, hi) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
        let len = m.text().len();
        let mut body = json!({"buffer": m.buffer(), "pattern": job.pattern, "regex": job.regex, "case": job.case});
        match job.kind {
            FindKind::Next => {
                body["from"] = json!(if job.wrapped { 0 } else { hi });
                body["limit"] = json!(1);
            }
            FindKind::Prev => {
                body["range"] = if job.wrapped { json!([hi, len]) } else { json!([0, lo]) };
                body["limit"] = json!(FIND_MAX_LIMIT);
            }
            FindKind::Replace if !job.checked => {
                body["range"] = json!([lo, hi]);
                body["groups"] = json!(job.expand);
                body["limit"] = json!(1);
            }
            FindKind::Replace => {
                body["from"] = json!(hi);
                body["limit"] = json!(1);
            }
            FindKind::ReplaceAll => {
                body["groups"] = json!(job.expand);
                body["limit"] = json!(FIND_MAX_LIMIT);
            }
        }
        let out = Outgoing { verb: "edit.find".into(), body: body.to_string(), op_id: None, deadline_ms: DEADLINE_MS };
        self.send(out, Req::Find { tab, job: Box::new(job) }, fx);
    }

    /// A page of `edit.find` for `job`.
    pub(super) fn on_find_reply(&mut self, tab: TabId, mut job: FindJob, rc: u8, body: &str, fx: &mut Vec<Effect>) {
        let info = |fx: &mut Vec<Effect>, text: &str| {
            fx.push(Effect::Notice { tab: Some(tab), notice: Notice::Message { level: Level::Info, text: text.to_string() } })
        };
        let reply = match reply_result(rc, body) {
            Ok(v) => match serde_json::from_value::<wire::FindReply>(v) {
                Ok(r) => r,
                Err(e) => return info(fx, &format!("bad edit.find reply: {e}")),
            },
            Err(r) => return info(fx, &r.message),
        };
        let Some(view_rev) = self.tab(tab).and_then(|t| t.mirror.as_ref()).map(Mirror::rev) else { return };
        if reply.rev != view_rev {
            // The text moved on: ask again once the pipeline is idle.
            job.found.clear();
            job.rev = None;
            return self.start_find(tab, job, fx);
        }
        let select = |c: &mut Controller, fx: &mut Vec<Effect>, m: &wire::MatchW| {
            if let Some(t) = c.tab_mut(tab) {
                t.editor.sel = Selection { anchor: m.start.offset, head: m.end.offset };
                t.editor.preferred_cells = None;
            }
            c.caret_moved(tab, fx);
        };
        match job.kind {
            FindKind::Next | FindKind::Replace if job.kind == FindKind::Next || job.checked => match reply.matches.first() {
                Some(m) => {
                    select(self, fx, m);
                    if job.wrapped {
                        info(fx, "Wrapped");
                    }
                }
                None if !job.wrapped => {
                    job.wrapped = true;
                    self.start_find(tab, job, fx);
                }
                None => info(fx, "No matches"),
            },
            FindKind::Replace => {
                // The selection check: replace it if it is exactly a match.
                job.checked = true;
                let sel = self.tab(tab).map(|t| t.editor.sel).unwrap_or(Selection { anchor: 0, head: 0 });
                let (lo, hi) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
                if let Some(m) = reply.matches.first().filter(|m| m.start.offset == lo && m.end.offset == hi && hi > lo) {
                    let text = if job.expand { expand_replacement(&job.replacement, m) } else { Some(job.replacement.clone()) };
                    let Some(text) = text else { return info(fx, "A referenced group is too long to replace") };
                    let op = ServerOp::ApplyAt { items: vec![(lo..hi, text)], expect_rev: reply.rev };
                    if let Err(e) = self.server_op(tab, op, job.intent.clone(), fx) {
                        return info(fx, &e);
                    }
                    if let Some(t) = self.tab_mut(tab) {
                        t.editor.sel = Selection { anchor: hi, head: hi };
                    }
                }
                self.start_find(tab, job, fx);
            }
            FindKind::Prev => {
                if job.rev.is_some_and(|r| r != reply.rev) {
                    job.found.clear();
                }
                job.rev = Some(reply.rev);
                job.found.extend(reply.matches);
                if reply.truncated
                    && let Some(next) = reply.next
                {
                    return self.find_page(tab, job, next, fx);
                }
                match job.found.last().cloned() {
                    Some(m) => {
                        select(self, fx, &m);
                        if job.wrapped {
                            info(fx, "Wrapped");
                        }
                    }
                    None if !job.wrapped => {
                        job.wrapped = true;
                        job.found.clear();
                        job.rev = None;
                        self.start_find(tab, job, fx);
                    }
                    None => info(fx, "No matches"),
                }
            }
            FindKind::ReplaceAll => {
                if job.rev.is_some_and(|r| r != reply.rev) {
                    if job.refound {
                        return info(fx, "The text kept changing; nothing was replaced");
                    }
                    job.refound = true;
                    job.found.clear();
                    job.rev = None;
                    return self.start_find(tab, job, fx);
                }
                job.rev = Some(reply.rev);
                job.found.extend(reply.matches);
                if job.found.len() > MAX_OPS_PER_TXN {
                    return info(fx, &format!("Too large for one undoable transaction (over {MAX_OPS_PER_TXN} matches); narrow the search or use a macro."));
                }
                if reply.truncated
                    && let Some(next) = reply.next
                {
                    return self.find_page(tab, job, next, fx);
                }
                if job.found.is_empty() {
                    return info(fx, "No matches");
                }
                let mut items = Vec::with_capacity(job.found.len());
                let mut bytes = 0usize;
                for m in &job.found {
                    let text = if job.expand { expand_replacement(&job.replacement, m) } else { Some(job.replacement.clone()) };
                    let Some(text) = text else { return info(fx, "A referenced group is too long to replace") };
                    bytes += text.len();
                    items.push((m.start.offset..m.end.offset, text));
                }
                if bytes > MAX_REQUEST_TEXT_BYTES {
                    return info(
                        fx,
                        &format!("Too large for one undoable transaction ({} matches, {bytes} bytes); narrow the search or use a macro.", items.len()),
                    );
                }
                let n = items.len();
                let op = ServerOp::ApplyAt { items, expect_rev: reply.rev };
                match self.server_op(tab, op, job.intent.clone(), fx) {
                    Ok(()) => info(fx, &format!("Replaced {n}")),
                    Err(e) => info(fx, &e),
                }
            }
            FindKind::Next => unreachable!("handled above"),
        }
    }

    /// The next page of a Prev / ReplaceAll search, resuming at `from`.
    fn find_page(&mut self, tab: TabId, job: FindJob, from: usize, fx: &mut Vec<Effect>) {
        let Some(m) = self.tab(tab).and_then(|t| t.mirror.as_ref()) else { return };
        let sel = self.tab(tab).map(|t| t.editor.sel).unwrap_or(Selection { anchor: 0, head: 0 });
        let (lo, hi) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
        let len = m.text().len();
        let mut body = json!({"buffer": m.buffer(), "pattern": job.pattern, "regex": job.regex, "case": job.case,
                              "from": from, "limit": FIND_MAX_LIMIT});
        if job.kind == FindKind::Prev {
            body["range"] = if job.wrapped { json!([hi, len]) } else { json!([0, lo]) };
        } else {
            body["groups"] = json!(job.expand);
        }
        let out = Outgoing { verb: "edit.find".into(), body: body.to_string(), op_id: None, deadline_ms: DEADLINE_MS };
        self.send(out, Req::Find { tab, job: Box::new(job) }, fx);
    }

    /// `edit.list` at start: offer recovered buffers no tab holds, once.
    pub(super) fn on_recovered_list(&mut self, rc: u8, body: &str, fx: &mut Vec<Effect>) {
        let Some(list) = reply_result(rc, body).ok().and_then(|v| serde_json::from_value::<wire::ListReply>(v).ok()) else { return };
        let held: Vec<String> = self.tabs.iter().filter_map(|t| t.mirror.as_ref().map(|m| m.buffer().to_string())).collect();
        let held_paths: Vec<String> = self.tabs.iter().filter_map(|t| t.path.clone()).collect();
        let held_rids: Vec<String> = self.x.values().filter_map(|x| x.recovery_id.clone()).collect();
        self.recovered_epoch = list.epoch.clone();
        self.recovered = list
            .buffers
            .into_iter()
            .filter(|b| b.recovered && b.holders.is_empty())
            .filter(|b| !held.contains(&b.buffer) && !held_rids.contains(&b.recovery_id))
            .filter(|b| b.path.as_ref().is_none_or(|p| !held_paths.contains(p)))
            .collect();
        if self.recovered.is_empty() {
            return;
        }
        let buffers = self
            .recovered
            .iter()
            .map(|b| RecoveredRow {
                buffer: b.buffer.clone(),
                path: b.path.clone(),
                name: b.name.clone().unwrap_or_else(|| format!("untitled ({})", b.recovery_id)),
                bytes: Some(b.bytes),
            })
            .collect();
        fx.push(Effect::Prompt(Prompt::Recovered { buffers }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_edit_core::pos::Point;

    fn m(text: &str, groups: Option<Vec<Option<&str>>>) -> wire::MatchW {
        let p = Point { offset: 0, line: 1, col: 1 };
        wire::MatchW {
            start: p,
            end: p,
            text: text.into(),
            text_truncated: false,
            groups: groups.map(|g| g.into_iter().map(|x| x.map(str::to_string)).collect()),
            groups_truncated: false,
        }
    }

    #[test]
    fn replacement_expansion() {
        let x = m("foo=bar", Some(vec![Some("foo"), Some("bar"), None]));
        assert_eq!(expand_replacement("$2=$1", &x).as_deref(), Some("bar=foo"));
        assert_eq!(expand_replacement("[$0] $$ ${1}x $3.", &x).as_deref(), Some("[foo=bar] $ foox ."));
        assert_eq!(expand_replacement("cost $", &x).as_deref(), Some("cost $"));
        assert_eq!(expand_replacement("$1", &m("a", None)), None);
        let mut t = m("long", None);
        t.text_truncated = true;
        assert_eq!(expand_replacement("$0", &t), None);
    }

    #[test]
    fn percentile_ranks() {
        let q: VecDeque<u64> = (1..=100).collect();
        let p = percentiles(&q);
        assert_eq!((p.p50, p.p95, p.p99, p.max), (50, 95, 99, 100));
        assert_eq!(percentiles(&VecDeque::new()), verbs::Percentiles::default());
    }
}
