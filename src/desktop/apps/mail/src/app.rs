//! Window, shell and the wiring between the three widgets.
//!
//! The data flow this slice exists to prove, in one place:
//!
//! ```text
//! VirtualListSelectionChanged ──> swap_body ──> body_view respawn
//!                                      │
//!                                      └──> mark_read ──> PendingRebinds
//!                                                              │
//!                                          virtual_list_changed(Updated) <┘
//!
//! VirtualListRowActivated ────> reply prefill ──> text_area
//! LinkActivated ──────────────> the app opens the URL; CTK never does
//! ```
//!
//! Selection drives the reader through a queued exclusive command rather than
//! directly from the observer, because the swap needs `&mut World` to flush
//! and time the spawn (see `reader::swap_body`).

use std::process::{Child, Command};
use std::sync::Arc;

use bevy::feathers::dark_theme::create_dark_theme;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, UiTheme};
use bevy::feathers::FeathersPlugins;
use bevy::picking::events::{Click, Pointer};
use bevy::picking::Pickable;
use bevy::prelude::*;
use ctk::prelude::*;
use ctk::theme::tokens;

use crate::compose::{reply_quote, reply_subject};
use crate::list::{MailListModel, ROW_HEIGHT};
use crate::probe::{self, ProbePlan};
use crate::reader::{
    describe_remote, live_remote_summary, swap_body, Corpus, PendingRebinds, Reader,
};
use crate::store::{FixtureStore, MailRowId, MessageStore, Summary};
use crate::IDENTITY;

/// Messages in the synthetic corpus.
///
/// Large enough that recycling is doing real work and a linear scan would show
/// up in the numbers, small enough to start instantly.
const CORPUS_SIZE: usize = 50_000;

const LIST_VIEWPORT: f32 = 560.0;

#[derive(Resource)]
pub struct Ui {
    pub list: Entity,
    pub status: Entity,
    pub compose_area: Entity,
    pub subject: Entity,
}

#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum ToolbarAction {
    Reply,
    RemoteContent,
    Report,
}

/// Most link launches allowed to be in flight at once.
///
/// `xdg-open` normally exits in milliseconds, so reaching this means launches
/// are not completing — a wedged handler, or a user leaning on the return key
/// over a list of links. Either way the answer is to stop starting processes,
/// not to keep queueing them behind a handler that is not consuming them.
const MAX_PENDING_LAUNCHES: usize = 32;

/// Browser processes launched for activated links, reaped without blocking.
#[derive(Resource, Default)]
struct Launched {
    children: Vec<Launch>,
    /// Failures seen so far, kept **across** frames.
    ///
    /// A child exits when it exits; two dead links clicked together routinely
    /// finish in different frames. Building the status line from one frame's
    /// observations meant the second failure overwrote the first and the user
    /// was told about one of them. Retained until the next activation, because
    /// that is the point at which the user has moved on and the line is theirs
    /// to claim again.
    failures: Vec<String>,
    /// Which activation currently owns the status line.
    ///
    /// Incremented by every link activation and stamped on the launches it
    /// starts. A completion whose stamp is older than this is reaped but not
    /// reported: its status line has already been taken over by a click the
    /// user made afterwards, and letting a stale failure reclaim it would put
    /// "Did not open A" over a freshly-opened B.
    ///
    /// The trade, stated rather than assumed: a failure from the previous
    /// activation is **dropped**, not queued. The status bar is one slot and
    /// the newest user action owns it; a report the user cannot connect to
    /// anything they just did is noise that trains them to ignore the line. If
    /// link results ever need to be durable, they need somewhere other than the
    /// status bar to live.
    generation: u64,
}

/// One in-flight `xdg-open`, kept with the href so a failed launch can name it.
struct Launch {
    child: Child,
    href: String,
    /// [`Launched::generation`] at the moment this launch started.
    generation: u64,
    /// Set when `try_wait` failed for a reason that is not going to improve.
    ///
    /// Such a launch is never waited on again — the syscall failed once and
    /// re-asking it sixty times a second is how a broken handle becomes a
    /// busy loop — and never reported again, because the user has already been
    /// told. It is nonetheless **kept**, because the alternative is worse than
    /// it looks: dropping the handle does not reap the process (`Child::drop`
    /// neither waits nor detaches), so a dropped entry frees a cap slot while
    /// the process it represents may still be running or sitting as a zombie.
    /// The cap exists to bound processes, so the thing it must count is
    /// precisely the one this app has lost track of.
    broken: bool,
}

pub fn run(plan: Option<ProbePlan>) {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: format!("{} · reader and composer", IDENTITY.display_name),
            name: Some(IDENTITY.app_id()),
            resolution: (1360, 880).into(),
            resizable: true,
            ..default()
        }),
        ..default()
    }))
    .add_plugins((
        FeathersPlugins,
        CtkThemePlugin::default(),
        // `DcsAppShellPlugin` already pulls in `ChromePlugin`, `DcsShellPlugin`
        // and the menu bar. Adding either of those alongside it panics at
        // startup ("plugin was already added"), so the shell is registered once
        // and only once.
        DcsAppShellPlugin,
        VirtualListPlugin,
        CtkBodyViewPlugin,
        CtkTextAreaPlugin,
    ))
    .init_resource::<PendingRebinds>()
    .init_resource::<Launched>()
    .add_systems(Startup, setup)
    .add_systems(Update, (drain_rebinds, reap_launched))
    .add_observer(on_selection_changed)
    .add_observer(on_row_activated)
    .add_observer(on_link_activated)
    .add_observer(on_toolbar_click);

    if let Some(plan) = plan {
        probe::install(&mut app, plan);
    }
    app.run();
}

fn setup(mut commands: Commands, mut theme: ResMut<UiTheme>, mut theme_state: ResMut<ThemeState>) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);

    let store: Arc<dyn MessageStore> = Arc::new(FixtureStore::synthetic(CORPUS_SIZE));
    commands.insert_resource(Corpus(store.clone()));

    let list = spawn_virtual_list(
        &mut commands,
        VirtualListProps::new(ROW_HEIGHT, LIST_VIEWPORT, "Messages")
            .selection_mode(SelectionMode::Single),
        MailListModel::new(store),
    );
    commands.entity(list.root).insert((
        ThemeBackgroundColor(tokens::SURFACE),
        Node {
            width: percent(100),
            height: percent(100),
            ..default()
        },
    ));

    let reader_host = commands
        .spawn((
            Node {
                width: percent(100),
                flex_grow: 1.0,
                min_height: px(0),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            ThemeBackgroundColor(tokens::SURFACE),
        ))
        .id();

    let subject = commands
        .spawn((
            Text::new("Select a message"),
            TextFont::from_font_size(15.0),
            ThemeTextColor(tokens::TEXT),
            Node {
                padding: UiRect::axes(px(12), px(8)),
                ..default()
            },
        ))
        .id();

    let compose = spawn_text_area(
        &mut commands,
        CtkTextAreaProps::new("", "Reply body")
            .visible_lines(6)
            .min_height(140.0),
    );

    let reader_column = commands
        .spawn(Node {
            width: percent(100),
            height: percent(100),
            flex_direction: FlexDirection::Column,
            min_width: px(0),
            ..default()
        })
        .add_children(&[subject, reader_host, compose.root])
        .id();

    let split = spawn_dcs_split(
        &mut commands,
        DcsSplitProps {
            first: list.root,
            second: reader_column,
            ratio: 0.38,
        },
    );

    let toolbar = spawn_toolbar(&mut commands);
    let folders = spawn_folders(&mut commands);
    let status = spawn_status_bar(&mut commands, "Ready");

    spawn_dcs_app_shell(
        &mut commands,
        DcsAppShellProps::new(DcsShellProps::new(
            toolbar,
            split.root,
            vec![DcsPanel::new("folders", "Folders", folders)],
            Vec::new(),
        ))
        .with_status_bar(status.root),
    );

    commands.insert_resource(Reader::new(reader_host));
    commands.insert_resource(Ui {
        list: list.root,
        status: status.text,
        compose_area: compose.input,
        subject,
    });
}

fn spawn_toolbar(commands: &mut Commands) -> Entity {
    let buttons = [
        ("Reply", ToolbarAction::Reply),
        ("Remote content", ToolbarAction::RemoteContent),
        ("Report", ToolbarAction::Report),
    ]
    .map(|(label, action)| {
        let button = toolbar_button(commands, label);
        commands.entity(button).insert(action);
        button
    });
    commands
        .spawn(Node {
            width: percent(100),
            height: px(38),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(8),
            padding: UiRect::horizontal(px(8)),
            ..default()
        })
        .add_children(&buttons)
        .id()
}

fn toolbar_button(commands: &mut Commands, label: &str) -> Entity {
    let text = commands
        .spawn((
            Text::new(label),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::TEXT),
            Pickable::IGNORE,
        ))
        .id();
    commands
        .spawn((
            Button,
            Pickable::default(),
            Node {
                height: px(28),
                min_width: px(72),
                padding: UiRect::horizontal(px(10)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            ThemeBackgroundColor(tokens::CONTROL),
        ))
        .add_child(text)
        .id()
}

fn spawn_folders(commands: &mut Commands) -> Entity {
    let items = ["Inbox", "Archive", "Sent", "Drafts", "Junk"].map(|name| {
        commands
            .spawn((
                Text::new(name),
                TextFont::from_font_size(13.0),
                ThemeTextColor(tokens::TEXT),
                Node {
                    padding: UiRect::axes(px(10), px(5)),
                    ..default()
                },
            ))
            .id()
    });
    commands
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .add_children(&items)
        .id()
}

fn on_selection_changed(
    event: On<VirtualListSelectionChanged>,
    ui: Option<Res<Ui>>,
    mut commands: Commands,
) {
    // Observers registered on the `App` are global: this fires for *every*
    // virtual list in the world, not only the message list. `RowId` is
    // per-list, so a second list — a folder tree, an attachment picker — that
    // emitted `RowId(0)` would land here and open message zero. The shell has
    // one list today, which is exactly why the check has to be written now:
    // the day a second one appears, nothing about this code will look wrong.
    if ui.is_none_or(|ui| event.list != ui.list) {
        return;
    }
    // Single-selection mode, so the first id is the selection; an empty vector
    // means the selection was cleared and the reader keeps what it has.
    let Some(&id) = event.selected.first() else {
        return;
    };
    // A placeholder id means the list realised a row the store had already
    // dropped (`list::MailListModel::row_id`); there is no message behind it to
    // show. The check lives in `MailRowId::new` rather than in each consumer,
    // which is the point of the type: an id that reaches `select` has already
    // been proved to be one a store could have issued.
    let Some(id) = MailRowId::new(id) else {
        return;
    };
    commands.queue(move |world: &mut World| {
        // The answer is the probe's business; a real selection that resolved to
        // nothing has already left the pane and heading alone, which is the
        // right thing for the user to see.
        select(world, id);
    });
}

/// Everything selecting a message does, in one place.
///
/// Called by the selection observer and by `probe::drive`. Keeping it as one
/// function is what makes the probe's numbers mean anything: when the probe
/// drove `swap_body` alone it measured a strict subset of the real path and
/// silently missed `update_subject`, which at the time carried a full-model
/// linear scan. A probe that exercises less than the app does is a probe that
/// reports the app as faster than it is.
///
/// The heading and the body come from **one** store read, and that is the whole
/// design of this function. They used to be two: `swap_body` fetched the body,
/// then `update_subject` fetched the summary, and a store that changed between
/// the two acquisitions could serve a heading and a body from different
/// messages. An earlier round narrowed that by threading the swap's answer back
/// out — which fixed the *declined-swap* case and left the *changed-message*
/// case, because both halves were still separate reads. `MessageStore::message`
/// is the actual fix: the store's own lock makes the pair a snapshot, which is
/// the only place that guarantee can come from.
///
/// `swap_body` still reports which message the pane ends up showing, because it
/// can decline — now for exactly one reason, a missing `Reader` resource. It
/// used to also decline when the pane already held `id`, and that fast path is
/// gone on purpose (`reader::swap_body`): an id says *which* message, never
/// which version of it. The heading follows the swap's answer rather than the
/// request regardless, because the answer is the one that came from the store
/// read that actually happened.
///
/// Returns whether the pane ends up showing `id`. Only `probe::drive` reads it,
/// and it needs to: a selection that resolved to no message is a frame that did
/// a fraction of the work, and counting it as a normal one reports the app as
/// faster than it is. `false` is the probe's cue to record a skip rather than a
/// measurement.
pub(crate) fn select(world: &mut World, id: MailRowId) -> bool {
    let Some(corpus) = world.get_resource::<Corpus>().cloned() else {
        return false;
    };
    let Some((summary, source)) = corpus.0.message(id) else {
        return false;
    };
    let Some(shown) = swap_body(world, id, source) else {
        return false;
    };
    // Only the snapshot's own summary can be trusted as the heading for the
    // body just installed; if the swap declined and left an older message up,
    // re-reading that one's summary would be a second unpaired read of exactly
    // the kind this function exists to avoid.
    if shown != id {
        return false;
    }
    update_subject(world, &summary);
    true
}

/// Write the header for the message the pane is showing.
///
/// Takes the summary rather than an id, for the reason given on [`select`]: a
/// second store read here is a second snapshot, and the heading's whole job is
/// to belong to the body beside it. There is no "summary has gone" arm any more
/// because there is no lookup that could miss — the summary arrived with the
/// body it describes.
///
/// Deliberately not deferred, and deliberately not subscribed: the header is
/// refreshed at selection and nowhere else, so a store that changed a shown
/// message's subject in place would leave the heading stale until the next
/// click. Not fixed here, with a reason — this slice has no store-change
/// notification path at all, the fixture corpus is immutable, and inventing one
/// for the header alone would be a second, narrower mechanism beside the one the
/// list already needs. It belongs with the JMAP change notifications that will
/// drive `virtual_list_changed`; the header should ride that, not precede it.
fn update_subject(world: &mut World, summary: &Summary) {
    let Some(ui) = world.get_resource::<Ui>() else {
        return;
    };
    let subject_entity = ui.subject;
    if let Some(mut text) = world.get_mut::<Text>(subject_entity) {
        text.0 = format!("{}  —  {}", summary.subject, summary.from);
    }
}

fn on_row_activated(
    event: On<VirtualListRowActivated>,
    ui: Option<Res<Ui>>,
    mut commands: Commands,
) {
    // Same reasoning as `on_selection_changed`, both for the list check and for
    // the placeholder check: activating a placeholder row has nothing to quote,
    // and a row id from another list names nothing here.
    if ui.is_none_or(|ui| event.list != ui.list) {
        return;
    }
    let Some(id) = MailRowId::new(event.row_id) else {
        return;
    };
    commands.queue(move |world: &mut World| prefill_reply(world, id));
}

/// Build the quoted reply and put it in the compose area.
///
/// Re-sanitises and re-projects the body rather than caching the reader's
/// projection. That is one extra pass on an explicit user action, and it keeps
/// the reader from having to own a projection it does not otherwise need.
fn prefill_reply(world: &mut World, id: MailRowId) {
    let Some(corpus) = world.get_resource::<Corpus>().cloned() else {
        return;
    };
    // Id-keyed, like `select`. This was the *second* copy of the 50 000-row
    // scan; removing the one in `update_subject` left this one behind, and it
    // is the worse of the two — it also fed a possibly-stale position back into
    // `summary(index)`, so a corpus that changed between the scan and the read
    // could have quoted one message under another's name.
    //
    // One read, for the same reason and with the sharpest consequence in the
    // app: `summary_by_id` then `body` are two acquisitions, and a store that
    // replaces the message between them produces a draft quoting the *new*
    // body under the *old* sender and subject. That is a misattributed quote
    // the user then sends under their own name, from a race nothing on screen
    // reveals.
    let Some((summary, source)) = corpus.0.message(id) else {
        // Say so. Returning quietly left the previous draft and its "Reply
        // prepared — …" status untouched, so a Reply to a message that had
        // vanished looked exactly like a Reply that worked, aimed at whichever
        // message the stale draft quoted.
        set_status(
            world,
            "That message is no longer available — nothing was quoted.".to_string(),
        );
        return;
    };
    let projection = project_body(&source.sanitize());
    let quoted = reply_quote(&summary.from, &projection);
    let subject = reply_subject(&summary.subject);

    let Some(ui) = world.get_resource::<Ui>() else {
        return;
    };
    let area = ui.compose_area;
    if let Some(mut editable) = world.get_mut::<bevy::text::EditableText>(area) {
        // `EditableText` has no value setter — `new()` is the only constructor
        // that takes text, and `clear()` the only writer. Replacement goes
        // through the editor itself, which is what CTK's own `restore_snapshot`
        // does (`ctk/src/text_area.rs:1332`). CTK's `process_text_area_edits`
        // notices the text no longer matches its policy snapshot and reconciles
        // it as a programmatic replacement, so the undo history and the
        // max-length clamp both stay honest without this app touching them.
        editable.editor_mut().set_text(&quoted);
    }
    // Through `set_status`, not a direct write: this is a fresh user-visible
    // result and has to *claim* the line, not merely occupy it. A direct write
    // left the generation where it was, so a link launched before the Reply
    // could fail afterwards and take the line back with "Did not open …" — the
    // user would see their reply silently replaced by a complaint about an
    // action they had already moved on from.
    set_status(world, format!("Reply prepared — {subject}"));
}

fn on_link_activated(
    event: On<LinkActivated>,
    mut launched: ResMut<Launched>,
    mut status: Query<&mut StatusText>,
    reader: Res<Reader>,
    ui: Res<Ui>,
) {
    // The observer is global, and activation reaches it deferred: a click in
    // message A can be queued in the frame that selects B, by which time A's
    // view has been despawned and replaced. Opening A's URL then would launch
    // a link from a message no longer on screen — the user's click and what
    // the click does would refer to different mail. `LinkActivated` names its
    // origin view, so the check is exact rather than a heuristic.
    if !reader.is_current_view(event.body_view) {
        return;
    }
    let href = event.href.clone();
    // A fresh click owns the status line, so past failures stop competing for
    // it. They have already been shown; keeping them would mean a link opened
    // now is reported under a complaint about one that failed minutes ago.
    // Clearing the vector is only half of that — the launches that produced it
    // are still running and would refill it — so the generation moves too, and
    // `reap_launched` reports only launches stamped with the current one.
    launched.failures.clear();
    launched.generation += 1;
    let generation = launched.generation;
    // "Handed to", not "Opened": `spawn` succeeding only means the process
    // started. Whether it opened anything is its exit status, which arrives
    // later — `reap_launched` corrects the line if the launch failed.
    let message = match open_url(&href, &mut launched, generation, |href| {
        open_command(href).spawn()
    }) {
        Ok(()) => format!("Opening {href}…"),
        Err(reason) => format!("Did not open {href}: {reason}"),
    };
    if let Ok(mut text) = status.get_mut(ui.status) {
        text.0 = message;
    }
}

/// Hand a link to the desktop.
///
/// CTK already refuses active schemes before a href can reach `LinkActivated`,
/// so this check is the second of two rather than the only one — but it is the
/// one that lives next to the process spawn, which is where an allow-list
/// belongs.
/// The allow-list, kept pure so tests can exercise it without spawning.
fn is_openable_scheme(href: &str) -> Result<(), String> {
    let scheme = href
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
        return Err(format!("scheme {scheme:?} is not opened by this app"));
    }
    Ok(())
}

/// Start a browser for `href`, or say why not.
///
/// `spawn` is a seam, not a generalisation: production passes
/// [`open_command`]`(href).spawn()` and nothing else ever will. It exists so the
/// cap test can drive this exact control path — allow-list, reap, count, refuse
/// — with a spawn that panics instead of one that opens a window. Testing the
/// refusal through the real function is the only way to prove the refusal is
/// *in* the real function, and the seam is what makes that affordable: move or
/// delete the cap check and the test still reaches the spawn and still fails,
/// but no browser opens on the way.
fn open_url(
    href: &str,
    launched: &mut Launched,
    generation: u64,
    spawn: impl FnOnce(&str) -> std::io::Result<Child>,
) -> Result<(), String> {
    is_openable_scheme(href)?;
    // Reap before counting. `reap_launched` runs once a frame, so without this
    // the cap sees every child that exited since the last run as still pending
    // — 32 helpers that all finished instantly would refuse the 33rd click and
    // blame a handler that had in fact done its job. The check is only honest
    // if the number it reads is current, and making it current costs one
    // non-blocking `try_wait` per entry.
    reap(launched);
    // Bounded, because this vector is only ever drained by children exiting.
    // Nothing here rate-limits activation, so a handler that never exits turns
    // every click into another permanent entry — an unbounded queue of
    // processes fed by the user's mouse. Refusing is visible; growing is not.
    if launched.children.len() >= MAX_PENDING_LAUNCHES {
        // Two different failures, and the user can act on one of them. A
        // handler that is merely slow will drain; handles this app can no
        // longer wait on will not, and saying "still pending" about those would
        // promise a recovery that is not coming.
        let unwaitable = launched
            .children
            .iter()
            .filter(|launch| launch.broken)
            .count();
        return Err(if unwaitable > 0 {
            format!(
                "{MAX_PENDING_LAUNCHES} launches are outstanding and {unwaitable} of them could not be waited on — this app has lost track of those processes"
            )
        } else {
            format!(
                "{MAX_PENDING_LAUNCHES} link launches are still pending — the desktop handler is not finishing them"
            )
        });
    }
    // No `--` separator, deliberately. xdg-open(1) is a shell script whose
    // argument loop rejects anything matching `-*` with "unexpected option"
    // and only honours `--` when `XDG_UTILS_ENABLE_DOUBLE_HYPEN` is set in the
    // environment (`/usr/bin/xdg-open:418`, `:1171`), so passing it defensively
    // makes *every* launch exit 1. The allow-list above is the stronger guard
    // anyway: a href it accepts begins with `http:`, `https:` or `mailto:`, so
    // it cannot begin with '-' and there is nothing for a separator to protect.
    match spawn(href) {
        Ok(child) => {
            launched.children.push(Launch {
                child,
                href: href.to_string(),
                generation,
                broken: false,
            });
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// The exact argv handed to the desktop, split out so a test can assert its
/// shape without spawning a browser.
fn open_command(href: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(href);
    command
}

/// Reap finished browser launches without blocking the frame, and correct the
/// status line for any that failed.
///
/// The status written at activation time can only say the launch *started*.
/// This is where the app finds out whether it worked — without it a broken
/// handler is indistinguishable from a working one, which is exactly how the
/// `--` separator above went unnoticed.
fn reap_launched(
    mut launched: ResMut<Launched>,
    mut status: Query<&mut StatusText>,
    ui: Option<Res<Ui>>,
) {
    if launched.children.is_empty() {
        return;
    }
    if reap(&mut launched) == 0 {
        return;
    }
    let Some(ui) = ui else {
        return;
    };
    // Every failure since the last activation, not just this frame's: a user
    // who clicked three dead links was being told about whichever one exited
    // last. The status line is narrow, so past two it becomes a count — still
    // true, still not silent.
    let failures = &launched.failures;
    let message = match failures.len() {
        1 => format!("Did not open {}", failures[0]),
        2 => format!("Did not open {} or {}", failures[0], failures[1]),
        n => format!("Did not open {} — and {} more", failures[0], n - 1),
    };
    if let Ok(mut text) = status.get_mut(ui.status) {
        text.0 = message;
    }
}

/// Wait on every launch that can be waited on, without blocking the frame.
///
/// Returns how many *new* failures the current generation owns, which is what
/// tells the caller whether the status line needs rewriting. Split out of
/// [`reap_launched`] so [`open_url`] can call it too: the cap it enforces is a
/// count of live processes, and a count taken before reaping is a count of live
/// processes plus everything that has finished since the last frame.
fn reap(launched: &mut Launched) -> usize {
    let mut fresh = 0usize;
    let Launched {
        children,
        failures,
        generation,
    } = launched;
    let current = *generation;
    // Report only what the current activation started. Older launches are still
    // reaped — they must be, or they stay zombies — but their outcome no longer
    // gets the status line, which a later click has since claimed. See
    // `Launched::generation` for why the older result is dropped rather than
    // queued.
    let mut note = |launch: &Launch, what: String| {
        if launch.generation == current {
            failures.push(what);
            fresh += 1;
        }
    };
    children.retain_mut(|launch| {
        if launch.broken {
            // Already reported, already given up on. Kept only so it keeps
            // counting against the cap — see `Launch::broken`.
            return true;
        }
        match launch.child.try_wait() {
            Ok(Some(exit)) => {
                if !exit.success() {
                    note(launch, format!("{} (xdg-open {exit})", launch.href));
                }
                false
            }
            Ok(None) => true,
            // A signal arriving during the wait is not a failed wait — it is a
            // wait that has not happened yet. Retaining is the whole cost of
            // retrying it, and the next frame is the retry.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => true,
            // `ECHILD` is the one wait failure that means the process is *gone*.
            // It says there is no such child to wait for, which happens when
            // something outside this app collected it — `SIGCHLD` inherited as
            // `SIG_IGN`, `SA_NOCLDWAIT`, or a process-wide reaper in whatever
            // embeds this — and none of those are conditions this app can fix or
            // needs to survive. The exit status is unknowable, but the *process*
            // is not outstanding, and the cap counts outstanding processes. So
            // this is the one wait error that frees the slot: marking it broken
            // would spend a permanent slot on something that no longer exists,
            // and thirty-two of those disable link opening for the life of the
            // app. Nothing is reported either — there is no failure to report,
            // only a launch whose ending this app did not get to see.
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => false,
            // Anything else and the launch is given up on — the user is told,
            // because its outcome is now unknowable and silence would leave
            // "Opening …" standing as if it had worked.
            //
            // Marked rather than dropped. An earlier version removed the entry,
            // on the reasoning that re-asking a permanently failing syscall
            // sixty times a second is worse than leaking one record. The first
            // half of that is right and the conclusion was wrong: `Child`'s
            // `Drop` neither waits nor detaches (std documents it as *not*
            // reaping), so dropping the handle does not end the process — it
            // only ends this app's knowledge of it, while freeing the cap slot
            // that was supposed to bound it. Keeping the entry costs one
            // `bool` and buys back the cap's meaning; the flag is what stops
            // the retry loop.
            Err(error) => {
                note(
                    launch,
                    format!("{} (could not be waited on: {error})", launch.href),
                );
                launch.broken = true;
                true
            }
        }
    });
    fresh
}

fn on_toolbar_click(
    click: On<Pointer<Click>>,
    actions: Query<&ToolbarAction>,
    mut commands: Commands,
) {
    let Ok(&action) = actions.get(click.entity) else {
        return;
    };
    commands.queue(move |world: &mut World| run_toolbar_action(world, action));
}

fn run_toolbar_action(world: &mut World, action: ToolbarAction) {
    let message = match action {
        ToolbarAction::Reply => match world.get_resource::<Reader>().and_then(|r| r.current) {
            Some(id) => {
                // `prefill_reply` writes its own status line.
                prefill_reply(world, id);
                return;
            }
            None => "Select a message first.".to_string(),
        },
        ToolbarAction::RemoteContent => live_remote_summary(world)
            .map(|summary| describe_remote(&summary))
            .unwrap_or_else(|| "No message open.".to_string()),
        ToolbarAction::Report => world
            .get_resource::<Reader>()
            .map(|reader| {
                format!(
                    "body swaps: {} · swap {} · sanitise {}",
                    reader.stats.swaps,
                    reader.stats.swap.summary(),
                    reader.stats.sanitize.summary()
                )
            })
            .unwrap_or_default(),
    };
    set_status(world, message);
}

/// Write the status line **and take ownership of it**.
///
/// Writing is the easy half. The line is also written asynchronously by
/// `reap_launched` when a link launch exits badly, and those launches outlive
/// the click that started them — so a writer that does not advance the
/// generation is a writer that a minutes-old failure can overrule. Ownership
/// belongs to whatever the user did last, which means every synchronous writer
/// claims it here rather than each remembering to. `reap_launched` deliberately
/// does not go through this function: it is *reporting* on the current
/// generation, not starting a new one.
pub fn set_status(world: &mut World, message: String) {
    if let Some(mut launched) = world.get_resource_mut::<Launched>() {
        launched.failures.clear();
        launched.generation += 1;
    }
    let Some(ui) = world.get_resource::<Ui>() else {
        return;
    };
    let status = ui.status;
    if let Some(mut text) = world.get_mut::<StatusText>(status) {
        text.0 = message;
    }
}

/// Tell the list about summary changes once per frame.
///
/// `mark_read` can fire on every selection, and one `Updated` trigger per
/// message would make the list do reconciliation work proportional to how fast
/// the user arrows through the mailbox. Coalescing to a single contiguous span
/// per frame keeps that flat.
///
/// Coalescing `min..max + 1` over indices that may not be contiguous is
/// deliberately over-broad, and cheap because of where CTK evaluates the hint:
/// `explicitly_updated` is tested per *realised* row inside the realise loop
/// (`ctk/src/virtual_list.rs:1115`, gated at `:1177`), so a span covering the
/// whole model still costs at most the realised window — 24 rows here — and
/// never a scan of the model. CTK's own
/// `off_window_updated_hint_does_not_scan_the_model` (`:1617`) is the test that
/// pins that. The worst case is therefore a handful of redundant rebinds in a
/// frame that marked two distant rows read, against ~6 ms of frame for
/// rebinding the entire window (`list.rs`, measured by differencing whole
/// frames — CTK's own histogram says 2 ms and is scoped to `Update`). Tracking
/// runs to avoid them would buy a fraction of that, and only in the frames
/// where a user marked two distant rows read at once.
fn drain_rebinds(
    mut pending: ResMut<PendingRebinds>,
    corpus: Option<Res<Corpus>>,
    ui: Option<Res<Ui>>,
    mut commands: Commands,
) {
    if pending.0.is_empty() {
        return;
    }
    let (Some(ui), Some(corpus)) = (ui, corpus) else {
        pending.0.clear();
        return;
    };
    // Identities in, positions out, and the conversion happens here rather than
    // where the rebind was raised. The queue is filled during body swaps and
    // emptied at the end of the frame; a position stored across that gap is a
    // position an insertion can reassign, so the hint would refresh whichever
    // message had moved into the slot and leave the one actually read still
    // showing as unread. An id that no longer resolves is simply dropped — its
    // row is gone, so there is nothing to rebind.
    let mut positions = pending.0.drain(..).filter_map(|id| corpus.0.index_of(id));
    let Some(first) = positions.next() else {
        return;
    };
    let (mut lowest, mut highest) = (first, first);
    for position in positions {
        lowest = lowest.min(position);
        highest = highest.max(position);
    }
    virtual_list_changed(
        &mut commands,
        ui.list,
        ChangeHint::Updated(lowest..highest + 1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_and_mail_schemes_are_opened() {
        for href in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<b>x</b>",
            "ftp://example.org/x",
            "no-scheme-at-all",
        ] {
            assert!(
                is_openable_scheme(href).is_err(),
                "{href} must not reach xdg-open"
            );
            // Also through the real entry point, so the allow-list cannot be
            // dropped from `open_url` while the pure test above still passes.
            // The spawn seam turns "nothing was spawned" from an inference about
            // ordering into an assertion the kernel enforces.
            let mut launched = Launched::default();
            assert!(open_url(href, &mut launched, 1, |href| {
                panic!("{href} reached the spawn despite a refused scheme")
            })
            .is_err());
            assert!(
                launched.children.is_empty(),
                "{href} must not have been spawned"
            );
        }
    }

    /// A harmless child that stays alive, used to saturate the launch list
    /// without opening anything.
    ///
    /// Two properties, both load-bearing. It has to actually *run*, because
    /// `open_url` reaps before it counts and a list of instantly-exiting
    /// children is an empty list by the time the cap is read — a test built on
    /// those sails past the refusal and spawns a real `xdg-open` into the
    /// developer's session. And it has to run *without a deadline*: a finite
    /// sleep reintroduces the same failure on a slow or suspended machine,
    /// where the fixture expires mid-test and the assertion that was supposed
    /// to prove a refusal opens a browser instead. `Reaped` is what makes an
    /// unbounded sleep safe to spawn.
    ///
    /// A spawn failure is a **panic, not a skip**. These fixtures used to hand
    /// back `None` and their tests printed "skipped" and passed, which is a
    /// false green in the only environment where it matters: a host that cannot
    /// spawn `/bin/sleep` reports the cap as proven while proving nothing. This
    /// app runs on a Linux desktop and nowhere else, so a missing `/bin/sleep`
    /// is a broken host, not a supported configuration — and a loud failure that
    /// names the reason beats a green run that quietly tested nothing.
    fn launch_running() -> Launch {
        let child = Command::new("/bin/sleep")
            .arg("infinity")
            .spawn()
            .expect("/bin/sleep must be spawnable: this gate cannot run without it");
        Launch {
            child,
            href: "https://example.org/held".to_string(),
            generation: 0,
            broken: false,
        }
    }

    /// A child that is gone almost immediately.
    ///
    /// Panics rather than skips, for the reason given on [`launch_running`].
    fn launch_exiting() -> Launch {
        let child = Command::new("/bin/true")
            .spawn()
            .expect("/bin/true must be spawnable: this gate cannot run without it");
        Launch {
            child,
            href: "https://example.org/done".to_string(),
            generation: 0,
            broken: false,
        }
    }

    /// A [`Launched`] whose children are killed and waited on when it goes out
    /// of scope, **including** when the scope ends in a panic.
    ///
    /// Cleanup written as the last statement of a test is cleanup that a failing
    /// assertion skips, and the fixtures here are unbounded sleeps: the failure
    /// case would leave thirty-two of them running for the life of the machine.
    /// A `Drop` impl is the only cleanup that survives the assertion it exists
    /// to protect.
    struct Reaped(Launched);

    impl Drop for Reaped {
        fn drop(&mut self) {
            for launch in &mut self.0.children {
                let _ = launch.child.kill();
                let _ = launch.child.wait();
            }
        }
    }

    /// The guard is built **first**, and every child is pushed into it.
    ///
    /// Saturating into a plain `Launched` and wrapping it at the end leaves the
    /// partial run unguarded: a spawn that fails at child twenty — a process
    /// limit, a transient fork failure — drops twenty unbounded sleepers on the
    /// floor with nobody to kill them. The window is small and the fixture is
    /// `sleep infinity`, which is exactly the combination that makes it worth
    /// closing.
    ///
    /// It cannot return "skipped" any more, and that is deliberate. When it
    /// could, a spawn failure at child twenty produced a passing test — so a
    /// regression that moved the guard back outside the loop would leak twenty
    /// sleepers and still go green, which is the exact failure this guard exists
    /// to prevent going unnoticed. `make` panics on a failed spawn now, and a
    /// panic still runs `Drop`, so the guard reaps what it holds on the way out.
    fn saturate(make: fn() -> Launch) -> Reaped {
        let mut held = Reaped(Launched::default());
        while held.0.children.len() < MAX_PENDING_LAUNCHES {
            held.0.children.push(make());
        }
        held
    }

    #[test]
    fn launches_are_bounded_so_a_wedged_handler_cannot_queue_forever() {
        let mut held = saturate(launch_running);
        // The real entry point, because the property under test is that
        // `open_url` *consults* the cap — a pure `saturated(&Launched)` predicate
        // would keep passing if the check were lifted out of the spawn path. The
        // spawn seam is what makes that affordable: if the cap regresses, this
        // test fails on the panic below instead of opening a browser window into
        // the developer's session, which the earlier version of this test would
        // have done.
        let error = open_url("https://example.org/one-too-many", &mut held.0, 1, |href| {
            panic!("{href} was spawned even though the launch list was saturated")
        })
        .expect_err("a saturated launch list must refuse rather than grow");
        assert!(error.contains("still pending"), "got: {error}");
        assert_eq!(
            held.0.children.len(),
            MAX_PENDING_LAUNCHES,
            "the refusal must happen before the spawn, not after it"
        );
    }

    #[test]
    fn a_finished_launch_stops_counting_against_the_cap() {
        // The cap bounds *live* processes. Counting handles that have already
        // exited would refuse a click on behalf of a desktop handler that did
        // its job — and the previous version of the test above pinned exactly
        // that behaviour, because it filled the list with `/bin/true`.
        let mut done = saturate(launch_exiting);
        // `try_wait` is non-blocking, so poll rather than assume the children
        // have been scheduled yet.
        for _ in 0..200 {
            reap(&mut done.0);
            if done.0.children.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            done.0.children.is_empty(),
            "exited children must not hold cap slots"
        );
    }

    #[test]
    fn an_unwaitable_launch_keeps_its_slot_and_is_reported_once() {
        // A `try_wait` that fails for a reason that will not improve leaves a
        // process this app can no longer account for. Dropping the handle would
        // free its slot while the process may still exist, so the entry stays —
        // silently, because the user has already been told.
        let mut launch = launch_exiting();
        launch.broken = true;
        let mut held = Reaped(Launched {
            children: vec![launch],
            failures: Vec::new(),
            generation: 0,
        });
        assert_eq!(reap(&mut held.0), 0, "a broken launch is not re-reported");
        assert_eq!(
            held.0.children.len(),
            1,
            "a broken launch keeps counting against the cap"
        );
        assert!(held.0.failures.is_empty());
    }

    #[test]
    fn a_child_someone_else_reaped_frees_its_slot_instead_of_spending_it() {
        // The one wait failure that must *not* be treated as "lost track of it".
        // Produced here for real rather than by setting a flag: the child is
        // collected behind `std`'s back, exactly as an inherited `SIG_IGN` on
        // `SIGCHLD` or a process-wide reaper in an embedding host would do it,
        // so `try_wait` gets a genuine `ECHILD` from the kernel.
        let launch = launch_exiting();
        let pid = launch.child.id() as libc::pid_t;
        let mut status = 0;
        // SAFETY: `pid` is this process's own child and has not been waited on
        // — `Child::try_wait` has not been called, and `Child::drop` does not
        // reap — so the pid cannot have been recycled onto another process.
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(waited, pid, "this test must collect the child itself");

        // Deliberately **not** a `Reaped`. That guard kills what it holds, and
        // this pid has already been collected — so if the arm under test
        // regressed and left the entry in place, the guard would signal a pid
        // the kernel is free to have reissued to an unrelated process. A handle
        // whose child someone else reaped must be dropped, never killed. Nothing
        // leaks either way: the process this entry names is already gone, which
        // is the whole point of the test.
        let mut launched = Launched {
            children: vec![launch],
            failures: Vec::new(),
            generation: 0,
        };
        assert_eq!(
            reap(&mut launched),
            0,
            "a launch whose ending was collected elsewhere is not a failure"
        );
        assert!(
            launched.children.is_empty(),
            "ECHILD means the process is gone; its slot must go with it"
        );
    }

    #[test]
    fn scheme_matching_is_case_insensitive() {
        // Deliberately does not call `open_url`: that spawns, and `cargo test`
        // in a live desktop session would dispatch the URL to the developer's
        // browser before the handle could be killed. A test must not open
        // windows. The allow-list is pure, so it can be checked directly.
        for href in [
            "HTTPS://example.org/",
            "Http://example.org/",
            "MAILTO:someone@example.org",
        ] {
            assert!(
                is_openable_scheme(href).is_ok(),
                "{href} must pass the allow-list regardless of case"
            );
        }
    }

    #[test]
    fn the_href_is_the_only_argument() {
        // Regression: an added `--` separator made every launch exit 1, because
        // xdg-open's argument loop rejects `-*` unless
        // `XDG_UTILS_ENABLE_DOUBLE_HYPEN` is set. Spawn succeeds either way, so
        // nothing but the argv shape catches it here.
        let command = open_command("https://example.org/a?b=c");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["https://example.org/a?b=c"]);
    }
}
