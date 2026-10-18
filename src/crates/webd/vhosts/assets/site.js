/**
 * DCS Site JavaScript
 * Marketing enhancements on top of base.js app shell
 * Particles, scroll reveal, footer year
 * Copyright © 2026 Mark Constable <mc@dcs.spa> (MIT License)
 */

const Site = {
    config: {
        particleCount: 20
    },

    // ============================================
    // FLOATING PARTICLES
    // ============================================

    initParticles() {
        const container = document.getElementById("particles");
        if (!container) return;

        for (let i = 0; i < this.config.particleCount; i++) {
            const particle = document.createElement("div");
            particle.className = "particle";
            particle.style.left = Math.random() * 100 + "%";
            particle.style.animationDelay = Math.random() * 15 + "s";
            particle.style.animationDuration = (10 + Math.random() * 10) + "s";
            container.appendChild(particle);
        }
    },

    // ============================================
    // SCROLL REVEAL ANIMATION
    // ============================================

    // Reveal-on-scroll: snap whatever is already on screen at load visible (no
    // slide-up), then animate a section one-way only as it scrolls into view.
    // (The old version animated everything already visible at load and re-fired on
    // every scroll / layout reflow.)
    initScrollReveal() {
        const els = document.querySelectorAll(".reveal");
        if (els.length === 0) return;
        const vh = window.innerHeight || document.documentElement.clientHeight;
        // Already in view at load → snap visible with NO entrance animation,
        // but restore the stylesheet transition afterwards so hover still animates.
        // (A permanent `transition:none` here would kill the hover lift too.)
        els.forEach(el => {
            if (el.getBoundingClientRect().top < vh) {
                el.style.transition = "none";
                el.classList.add("active");
                void el.offsetWidth;      // force reflow so the final state paints instantly
                el.style.transition = "";  // hand transitions back to the stylesheet
            }
        });
        if (!("IntersectionObserver" in window)) {
            els.forEach(el => el.classList.add("active"));
            return;
        }
        // The rest animate one-way, only as they scroll into view.
        const io = new IntersectionObserver(entries => {
            entries.forEach(en => {
                if (en.isIntersecting) { en.target.classList.add("active"); io.unobserve(en.target); }
            });
        }, { rootMargin: "0px 0px -8% 0px" });
        els.forEach(el => { if (!el.classList.contains("active")) io.observe(el); });
    },

    // ============================================
    // DYNAMIC YEAR IN FOOTER
    // ============================================

    initFooterYear() {
        const yearSpan = document.getElementById("year");
        if (yearSpan) {
            yearSpan.textContent = new Date().getFullYear();
        }
    },

    // ============================================
    // TOUR DECK (sidebar slide-deck)
    // Independent of the outer panel carousel.
    // State is kept in closure — it does NOT touch base-state.
    // ============================================

    initDeck() {
        const deck = document.querySelector('.deck');
        if (!deck) return;

        const track = deck.querySelector('.deck-track');
        const slides = deck.querySelectorAll('.deck-slide');
        const dots = deck.querySelectorAll('.deck-dot');
        const counter = deck.querySelector('.deck-current');
        const count = slides.length;
        if (!track || !count) return;

        let idx = 0;
        deck.style.setProperty('--deck-count', count);

        const render = () => {
            deck.style.setProperty('--deck-idx', idx);
            slides.forEach((s, i) => s.classList.toggle('active', i === idx));
            dots.forEach((d, i) => d.classList.toggle('active', i === idx));
            if (counter) counter.textContent = String(idx + 1).padStart(2, '0');
        };

        const goto = (n) => {
            idx = ((n % count) + count) % count;  // wrap both directions
            render();
        };

        deck.addEventListener('click', (e) => {
            const dir = e.target.closest('[data-deck-dir]');
            if (dir) { goto(idx + (dir.dataset.deckDir === 'next' ? 1 : -1)); return; }
            const dot = e.target.closest('[data-deck-goto]');
            if (dot) { goto(parseInt(dot.dataset.deckGoto, 10)); }
        });

        deck.addEventListener('keydown', (e) => {
            if (e.key === 'ArrowLeft')  { goto(idx - 1); e.preventDefault(); }
            if (e.key === 'ArrowRight') { goto(idx + 1); e.preventDefault(); }
            if (e.key === 'Home')       { goto(0); e.preventDefault(); }
            if (e.key === 'End')        { goto(count - 1); e.preventDefault(); }
        });

        render();
    },

    // ============================================
    // COLLAPSIBLE NAV TREE (treeview.mix tv_*)
    // The chevron <button>.tv-chev toggles .tv-open on its .tv-branch (CSS does the
    // ▸→▾ rotate + grid collapse). State persists in localStorage by the namespaced
    // data-tv-key ("<tree>:<id>"), so it survives full reloads (e.g. /category/, which
    // the boost shim does NOT boost). The active path (ancestors of the link matching
    // the current URL) is a HARD FLOOR — always open, ignoring any stored "collapsed" —
    // so a load can never hide the page you're on. Server renders every branch open, so
    // a no-JS client keeps a fully expanded, navigable tree.
    // ============================================

    TV_KEY: "dcs:tv:collapsed",

    tvLoad() {
        try { return JSON.parse(localStorage.getItem(this.TV_KEY) || "{}") || {}; }
        catch (e) { return {}; }
    },
    tvSave(m) {
        try { localStorage.setItem(this.TV_KEY, JSON.stringify(m)); } catch (e) { /* quota/denied */ }
    },
    tvSetChev(li, open) {
        const c = li.querySelector(":scope > .tv-row > .tv-chev");
        if (c) c.setAttribute("aria-expanded", open ? "true" : "false");
    },

    // Reconcile server-rendered (all-open) state with stored collapses, keeping the
    // active path open. Suppress the collapse animation on this first pass (.tv-ready
    // gates the transition) so stored-closed branches snap shut without a flash-animate.
    applyTreeState() {
        const trees = document.querySelectorAll(".tv-tree");
        if (!trees.length) return;
        // Active path: ancestors of the active link — either its href matches the
        // current page (pathname-based nav: pages/categories) OR it carries
        // aria-current="page" (the server's active marker for query-string links, e.g.
        // the mail Folders tree's /pim/mail?box= folders). Without the aria-current arm,
        // a stored collapse of an ancestor (e.g. Trash) would re-hide the active folder.
        const active = new Set();
        document.querySelectorAll(".tv-tree .tv-link").forEach((a) => {
            if (a.getAttribute("href") !== location.pathname && a.getAttribute("aria-current") !== "page") return;
            let li = a.closest(".tv-branch");
            while (li) {
                const k = li.getAttribute("data-tv-key");
                if (k) active.add(k);
                li.classList.add("tv-open");
                this.tvSetChev(li, true);
                li = li.parentElement ? li.parentElement.closest(".tv-branch") : null;
            }
        });
        // Stored collapses apply to every NON-active branch.
        const m = this.tvLoad();
        document.querySelectorAll(".tv-branch[data-tv-key]").forEach((li) => {
            const k = li.getAttribute("data-tv-key");
            if (m[k] && !active.has(k)) {
                li.classList.remove("tv-open");
                this.tvSetChev(li, false);
            }
        });
        // Enable transitions for subsequent user toggles (next frame, post-snap).
        requestAnimationFrame(() => trees.forEach((t) => t.classList.add("tv-ready")));
    },

    initTree() {
        // One delegated listener (added once at load; survives boosted swaps since the
        // sidebar nav DOM persists). Fires only for the chevron button, so clicking the
        // link still navigates.
        document.addEventListener("click", (e) => {
            const btn = e.target.closest(".tv-chev");
            if (!btn) return;
            const li = btn.closest(".tv-branch");
            if (!li) return;
            e.preventDefault();
            const open = li.classList.toggle("tv-open");
            btn.setAttribute("aria-expanded", open ? "true" : "false");
            const key = li.getAttribute("data-tv-key");
            if (!key) return;
            const m = this.tvLoad();
            if (open) { delete m[key]; } else { m[key] = 1; }
            this.tvSave(m);
        });
        // A boosted nav (/page/, /post/, /admin, /pim/) patches only #content and never
        // re-fires DOMContentLoaded, but the persistent sidebar nav stays as-is — so the
        // shell dispatches cms:swapped after each swap for us to re-reconcile: re-open the
        // NEW active path (hard floor) while honouring stored collapses. Idempotent.
        document.addEventListener("cms:swapped", () => this.applyTreeState());
        this.applyTreeState();
    },

    // ============================================
    // CONTEXTUAL SIDEBAR PANELS  (POC: a sidebar panel drives a content app)
    // A content view ships an extra carousel panel for either sidebar by emitting,
    // anywhere in #content:
    //   <template data-dcs-panel="left" data-dcs-panel-id="mail-folders"
    //             data-dcs-panel-title="Folders" data-dcs-panel-hides="[data-mail-tabs]">
    //     …panel HTML (e.g. a .cms-nav folder list)…
    //   </template>
    // On load and after every boosted swap (cms:swapped) we reconcile: a declared
    // template is MOUNTED as a real carousel panel (+ a matching dot) and the carousel
    // surfaces it on first mount; a previously-mounted panel whose template is GONE
    // (you navigated out of the app) is removed and the carousel snaps home.
    // WHY here and not the boost shim: the folder list is SERVER-rendered (so it carries
    // live state and works with no JS via the in-content fallback), but it lives in the
    // SIDEBAR, which the boost shim deliberately CANNOT patch (its write surface is
    // #content-only, a security boundary). So the mount/teardown is a client-side mirror
    // of a server-rendered <template>. data-dcs-panel-hides drops the view's in-content
    // no-JS fallback (the folder tabs) once the panel is up. This is the substrate
    // pattern for "a sidebar panel driving a content application"; the mail Folders
    // panel (→ /pim/mail) is the first consumer.
    // ============================================

    // Surface a panel in the carousel WITHOUT persisting it as the user's choice.
    // Context panels are TRANSIENT (mounted per-view), so only the user's pick among
    // the STATIC panels is persisted — visiting an app never overwrites it, and leaving
    // returns them to where they were. base.js setPanel always persists, so we snapshot
    // the stored index and restore it immediately after the move.
    surfacePanel(side, idx, instant) {
        const sb = document.querySelector('.sidebar-' + side);
        if (!sb) return;
        const track = sb.querySelector('.panel-track');
        if (!track) return;
        const keep = Base.state()[side + 'Panel'];      // the user's real (static) choice
        if (instant) {
            track.style.transition = 'none';
            Base.setPanel(side, idx);
            track.offsetHeight;                         // commit the jump before re-enabling
            track.style.transition = '';
        } else {
            Base.setPanel(side, idx);
        }
        Base.state({ [side + 'Panel']: keep === undefined ? 0 : keep });   // un-clobber storage
    },

    // Make the dot row for `side` match the live panel count: append/remove trailing
    // dots (the static shell.html dots stay), renumber data-panel, and re-mark active
    // from persisted state. Base.setPanel re-asserts active when it runs after us.
    rebuildDots(side) {
        const sb = document.querySelector('.sidebar-' + side);
        if (!sb) return;
        const track = sb.querySelector('.panel-track');
        const dots = sb.querySelector('.carousel-dots');
        if (!track || !dots) return;
        const n = track.querySelectorAll('.panel').length;
        let dl = dots.querySelectorAll('.carousel-dot');
        while (dl.length < n) {
            const d = document.createElement('button');
            d.className = 'carousel-dot';
            d.setAttribute('data-sidebar', side);
            dots.appendChild(d);
            dl = dots.querySelectorAll('.carousel-dot');
        }
        while (dl.length > n) {
            dl[dl.length - 1].remove();
            dl = dots.querySelectorAll('.carousel-dot');
        }
        const cur = window.Base ? ((Base.state()[side + 'Panel']) || 0) : 0;
        const active = n ? (((cur % n) + n) % n) : 0;
        dl.forEach((d, i) => { d.setAttribute('data-panel', i); d.classList.toggle('active', i === active); });
    },

    syncContextPanels(instant) {
        if (!window.Base) return;
        ['left', 'right'].forEach((side) => {
            const sb = document.querySelector('.sidebar-' + side);
            if (!sb) return;
            const track = sb.querySelector('.panel-track');
            const dots = sb.querySelector('.carousel-dots');
            if (!track || !dots) return;

            // Templates declared in the freshly-rendered content for THIS side.
            const tpls = {};
            document.querySelectorAll('#content template[data-dcs-panel="' + side + '"]').forEach((t) => {
                const id = t.getAttribute('data-dcs-panel-id');
                if (id) tpls[id] = t;
            });
            // Context panels currently mounted in this carousel.
            const mounted = {};
            track.querySelectorAll('.panel-context[data-dcs-panel-id]').forEach((p) => {
                mounted[p.getAttribute('data-dcs-panel-id')] = p;
            });

            let changed = false, newId = null, removedAny = false;

            Object.keys(tpls).forEach((id) => {
                const t = tpls[id];
                const sig = t.innerHTML;   // template signature: changes only when the server content does
                if (mounted[id]) {
                    // Refresh ONLY when the server content actually changed (folder switch),
                    // NOT on a same-content re-sync (e.g. a dt-region sort/page partial fires
                    // cms:swapped too) — re-cloning would wipe in-panel UI state like an
                    // expanded folder branch.
                    if (mounted[id].dataset.dcsSig !== sig) {
                        const c = mounted[id].querySelector('.panel-content');
                        if (c) c.replaceChildren(t.content.cloneNode(true));
                        mounted[id].dataset.dcsSig = sig;
                    }
                } else {
                    const panel = document.createElement('div');
                    panel.className = 'panel panel-context';
                    panel.setAttribute('data-dcs-panel-id', id);
                    panel.dataset.dcsSig = sig;
                    const title = document.createElement('div');
                    title.className = 'panel-title';
                    title.textContent = t.getAttribute('data-dcs-panel-title') || '';
                    const content = document.createElement('div');
                    content.className = 'panel-content';
                    content.appendChild(t.content.cloneNode(true));
                    panel.appendChild(title);
                    panel.appendChild(content);
                    track.appendChild(panel);
                    changed = true; newId = id;
                }
                // Drop the in-content no-JS fallback the panel supersedes. Inline
                // display:none (not [hidden]) so it beats an author display (the tabs
                // are .row{display:flex}, which overrides the [hidden] UA rule). Content
                // re-renders fresh each nav, so re-hiding every sync is all we need.
                const hide = t.getAttribute('data-dcs-panel-hides');
                if (hide) {
                    try { document.querySelectorAll('#content ' + hide).forEach((el) => { el.style.display = 'none'; }); }
                    catch (e) { /* a future consumer's malformed selector must not break the reconcile */ }
                }
            });

            // Remove mounted context panels whose template is gone (left the app).
            Object.keys(mounted).forEach((id) => {
                if (!tpls[id]) { mounted[id].remove(); changed = true; removedAny = true; }
            });

            if (changed) this.rebuildDots(side);

            // A fresh mount surfaces the app's panel on arrival (NOT on a same-app
            // refresh, so a user who flipped to another panel mid-session isn't yanked).
            // `instant` (initial page load) snaps without the slide so a hard load of
            // /pim/mail doesn't sweep the whole carousel; a boosted nav animates normally.
            if (newId) {
                // Remember the user's pre-app STATIC panel so teardown can return to it —
                // even if they manually click the context panel's dot mid-session (which
                // base.js WOULD persist as the context index). Captured once per entry,
                // before surfacePanel (which doesn't persist the auto-switch anyway).
                this._dcsReturn = this._dcsReturn || {};
                if (this._dcsReturn[side] === undefined) this._dcsReturn[side] = (Base.state()[side + 'Panel']) || 0;
                const panels = Array.from(track.querySelectorAll('.panel'));
                const idx = panels.findIndex((p) => p.getAttribute('data-dcs-panel-id') === newId);
                if (idx >= 0) this.surfacePanel(side, idx, instant);   // transient: doesn't persist
            } else if (removedAny) {
                // Left the app: the carousel may be showing the now-removed context panel.
                // Return to the persisted static choice; if that index is out of range (the
                // user manually selected the context panel, so base.js persisted ITS index),
                // fall back to the remembered pre-app panel, then 0.
                const count = track.querySelectorAll('.panel').length;
                let keep = Base.state()[side + 'Panel'];
                if (keep === undefined || keep >= count) {
                    keep = (this._dcsReturn && this._dcsReturn[side] !== undefined) ? this._dcsReturn[side] : 0;
                }
                if (this._dcsReturn) delete this._dcsReturn[side];
                Base.setPanel(side, ((keep % count) + count) % count);
            }
        });
        if (window.lucide && lucide.createIcons) lucide.createIcons();   // render cloned icons
        // A mounted context panel may carry a collapsible .tv-tree (the mail Folders
        // tree). Its collapse/active reconcile + transition-enable is owned by
        // applyTreeState, which ran on this same cms:swapped BEFORE the panel was
        // mounted — so re-run it now the tree is in the DOM. Idempotent; no-op without a tree.
        if (this.applyTreeState) this.applyTreeState();
    },

    initContextPanels() {
        // Same cms:swapped reconcile pattern as the nav tree: the sidebar persists across
        // boosted swaps, so we re-mirror the (possibly new) content's declared panels.
        document.addEventListener('cms:swapped', () => this.syncContextPanels(false));
        this.syncContextPanels(true);   // initial load: snap, don't sweep the carousel
    },

    // ============================================
    // CONTEXT MENU  (contextmenu.mix cm_*)
    // Right-click a [data-cm-menu] target → clone its <template id="cm-<menuId>"> into a
    // floating .cm-menu at the cursor (clamped to the viewport). Each item's {key} URL
    // templates are resolved from the target's data-cm-<key> payload AT CLONE TIME — NOT
    // on click — because md_script (modals.mix) listens in CAPTURE phase, so a fill-in-on-
    // click would be too late to set data-modal-src before it fires. A document-level
    // delegated listener: the live targets are SIDEBAR clones (outside #content) of the
    // server-rendered folder tree, so a content-scoped listener would miss them. Closes on
    // Escape / outside-click / scroll / resize / cms:swapped (a panel re-clone must not
    // leave a menu pointing at a dead target).
    // ============================================

    _cm: null,
    _cmTarget: null,

    cmClose() {
        if (this._cm) { this._cm.remove(); this._cm = null; }
        this._cmTarget = null;
    },

    // data-cm-<key> dataset accessor. Mirrors the browser's data-* → dataset camelCasing
    // for "cm-<k>" (each "-x" → "X", as the DOM does), so a hyphenated payload key like
    // "parent-id" (data-cm-parent-id → dataset.cmParentId) resolves correctly — not just
    // the single-token folder keys (id/key/name/role → cmId/cmKey/cmName/cmRole).
    _cmDs(ds, k) {
        const key = ('cm-' + k).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
        return ds[key] != null ? ds[key] : '';
    },

    // Substitute {key} placeholders in a URL template from the target payload, URL-encoding
    // each value (for a URL/attr). The {!key} sigil splices the value RAW (no encode) — for a
    // value that is ALREADY a complete URL (e.g. an auto-generated row-action href bound whole
    // into data-cm-<name>, where the per-row href lambda already produced the finished URL).
    // An unknown key → "".
    cmSubst(tpl, ds) {
        return String(tpl).replace(/\{(!?)([a-z0-9-]+)\}/gi, (_, raw, k) => (raw ? this._cmDs(ds, k) : encodeURIComponent(this._cmDs(ds, k))));
    },

    // Like cmSubst but RAW (no URL-encoding) — for a form-field VALUE, which the browser
    // encodes itself on submit (double-encoding would corrupt it). Accepts {key} and {!key}
    // identically (both raw) so a template is safe in either substituter.
    cmSubstRaw(tpl, ds) {
        return String(tpl).replace(/\{!?([a-z0-9-]+)\}/gi, (_, k) => this._cmDs(ds, k));
    },

    // A one-click POST item (data-cm-post + data-cm-fields): build a temporary hidden form
    // in document.body (NOT in the menu — closing the menu would detach an in-menu form
    // before its native submit fires) and submit it → full-nav POST → 303 → flash. Fields
    // are real <input>s, resolved RAW from the bound target. Same-origin path-local only
    // (a leading single "/") so a template can't post to an arbitrary external action.
    cmSubmitPost(el) {
        const ds = this._cmTarget ? this._cmTarget.dataset : {};
        // Optional native-dialog gate (data-cm-confirm) so a destructive one-click POST (e.g. a
        // row Delete moved off the inline column) keeps its confirmation. Abort on cancel.
        const confirmText = el.getAttribute('data-cm-confirm');
        if (confirmText && !window.confirm(confirmText)) return;
        // Validate the RESOLVED action (after {key} substitution), not the raw template — a
        // template like "/{path}" could otherwise resolve to "//evil" (protocol-relative,
        // cross-origin). Require a single leading "/", rejecting "//" and "/\" (browsers
        // treat "/\" as protocol-relative too).
        const action = this.cmSubstRaw(el.getAttribute('data-cm-post') || '', ds);
        if (action.charAt(0) !== '/' || action.charAt(1) === '/' || action.charAt(1) === '\\') return;
        const form = document.createElement('form');
        // setAttribute (not form.action=/form.method=): a hidden field named "action" (the
        // house POST convention) CLOBBERS the form.action property getter/setter, so use the
        // attribute API which is immune. Likewise submit via the prototype method below.
        form.setAttribute('method', 'post');
        form.setAttribute('action', action);
        form.style.display = 'none';
        (el.getAttribute('data-cm-fields') || '').split('&').forEach((pair) => {
            if (!pair) return;
            const eq = pair.indexOf('=');
            const input = document.createElement('input');
            input.type = 'hidden';
            input.name = eq >= 0 ? pair.slice(0, eq) : pair;
            input.value = this.cmSubstRaw(eq >= 0 ? pair.slice(eq + 1) : '', ds);
            form.appendChild(input);
        });
        document.body.appendChild(form);
        // A field named "submit" would clobber form.submit() — call the prototype method.
        HTMLFormElement.prototype.submit.call(form);
    },

    // Resolve one cloned item against the target payload: apply the show/disable gates, then
    // turn the data-cm-src/href templates into real data-modal-src/data-modal-open/href.
    cmResolveItem(a, ds) {
        // show_if="key=value": hide the whole <li> unless the target's data-cm-<key> matches.
        const sif = a.getAttribute('data-cm-show-if');
        if (sif) {
            const eq = sif.indexOf('=');
            const k = eq >= 0 ? sif.slice(0, eq) : sif;
            const want = eq >= 0 ? sif.slice(eq + 1) : '';
            if (this._cmDs(ds, k) !== want) {
                const li = a.closest('.cm-li');
                if (li) li.style.display = 'none';
                return;                  // hidden — no need to resolve anything else
            }
        }
        const dif = a.getAttribute('data-cm-disable-if');
        if (dif && this._cmDs(ds, dif)) {
            a.classList.add('cm-disabled');
            a.setAttribute('aria-disabled', 'true');
            a.removeAttribute('href');
            return;                      // leave templates unresolved; clicks no-op
        }
        const src = a.getAttribute('data-cm-src');
        if (src) {
            a.setAttribute('data-modal-src', this.cmSubst(src, ds));
            const m = a.getAttribute('data-cm-modal');
            if (m) a.setAttribute('data-modal-open', m);
        }
        const href = a.getAttribute('data-cm-href');
        if (href) a.setAttribute('href', this.cmSubst(href, ds));
    },

    cmOpen(tpl, target, x, y) {
        this.cmClose();
        const wrap = document.createElement('div');
        wrap.className = 'cm-menu';
        wrap.setAttribute('role', 'menu');
        wrap.appendChild(tpl.content.cloneNode(true));
        const ds = target.dataset;
        wrap.querySelectorAll('a.cm-item').forEach((a) => this.cmResolveItem(a, ds));
        // An icon toolbar (cm_menu opts.toolbar) REPLACES the text rows when it fits: mark the
        // wrap so the CSS hides the duplicate <ul> (XOR). On a narrow viewport a CSS @media
        // breakpoint restores the rows (the PRIMARY responsive path).
        const tb = wrap.querySelector('.cm-toolbar');
        if (tb) wrap.classList.add('cm-has-toolbar');
        document.body.appendChild(wrap);
        this._cm = wrap;
        this._cmTarget = target;
        // Render icons BEFORE measuring: lucide swaps each <i> for a sized <svg>, so a width
        // taken earlier would miss the toolbar's real extent (the ordering bug this fixes).
        if (window.lucide && lucide.createIcons) lucide.createIcons();
        // SECONDARY fallback to the @media breakpoint: when .cm-has-toolbar hides the <ul>, the
        // menu sizes to the toolbar but is capped at the .cm-menu max-width; the toolbar's
        // flex:none buttons then overflow their own box (scrollWidth > clientWidth) and we drop
        // back to the text rows. (5–6 icons fit a desktop popover; a longer toolbar overflows.)
        if (tb && tb.scrollWidth > tb.clientWidth + 1) wrap.classList.add('cm-toolbar-off');
        // Position at the cursor, clamped into the viewport (4px gutter) — measured AFTER the
        // icon render + the toolbar/rows decision, so the size is final.
        const mw = wrap.offsetWidth, mh = wrap.offsetHeight;
        const vw = window.innerWidth, vh = window.innerHeight;
        let px = x, py = y;
        if (px + mw > vw - 4) px = Math.max(4, vw - mw - 4);
        if (py + mh > vh - 4) py = Math.max(4, vh - mh - 4);
        wrap.style.left = px + 'px';
        wrap.style.top = py + 'px';
        // Flyouts open leftward when the menu sits in the right half (avoids overflow).
        if (px > vw * 0.55) wrap.classList.add('cm-flip-left');
        // First-focus the first VISIBLE enabled item (cmItems filters by offsetParent), so a
        // hidden toolbar/text duplicate under the XOR toggle never steals focus.
        const first = this.cmItems()[0];
        if (first) first.focus();
    },

    // Currently-focusable items (enabled + actually visible — a closed submenu's items have
    // no offsetParent, so they're excluded until the flyout is open).
    cmItems() {
        if (!this._cm) return [];
        return Array.prototype.slice
            .call(this._cm.querySelectorAll('a.cm-item:not(.cm-disabled)'))
            .filter((a) => a.offsetParent !== null);
    },

    cmFocusRel(dir) {
        const items = this.cmItems();
        if (!items.length) return;
        let i = items.indexOf(document.activeElement);
        i = (i + dir + items.length) % items.length;
        items[i].focus();
    },

    initContextMenu() {
        document.addEventListener('contextmenu', (e) => {
            const t = e.target.closest('[data-cm-menu]');
            if (!t) return;
            const tpl = document.getElementById('cm-' + t.getAttribute('data-cm-menu'));
            if (!tpl || !tpl.content) return;
            e.preventDefault();
            this.cmOpen(tpl, t, e.clientX, e.clientY);
        });
        document.addEventListener('click', (e) => {
            if (!this._cm) return;
            // Swallow a click on a disabled item (no nav, keep the menu open).
            const dis = e.target.closest('.cm-disabled');
            if (dis && this._cm.contains(dis)) { e.preventDefault(); e.stopPropagation(); return; }
            // One-click POST item: build + submit a body-level form (decoupled from the
            // menu so closing it doesn't detach the form), then close.
            const postEl = e.target.closest('[data-cm-post]');
            if (postEl && this._cm.contains(postEl)) {
                e.preventDefault();
                this.cmSubmitPost(postEl);
                this.cmClose();
                return;
            }
            if (this._cm.contains(e.target)) {
                // A real item was clicked: md_script (capture) / the boost shim already
                // handled it — just close, unless it's a submenu parent (keep open).
                const item = e.target.closest('a.cm-item');
                if (item && !item.classList.contains('cm-has-submenu')) this.cmClose();
                return;
            }
            this.cmClose();                  // outside click
        });
        document.addEventListener('keydown', (e) => {
            if (!this._cm) return;
            if (e.key === 'Escape') {
                e.preventDefault();
                const t = this._cmTarget; this.cmClose();
                if (t && t.focus) { try { t.focus(); } catch (_) { /* gone */ } }
                return;
            }
            if (e.key === 'ArrowDown') { e.preventDefault(); this.cmFocusRel(1); return; }
            if (e.key === 'ArrowUp')   { e.preventDefault(); this.cmFocusRel(-1); return; }
            if (e.key === 'Enter' || e.key === ' ') {
                const a = document.activeElement;
                if (a && a.classList && a.classList.contains('cm-item')) { e.preventDefault(); a.click(); }
                return;
            }
            if (e.key === 'ArrowRight') {
                const a = document.activeElement;
                const sm = a && a.parentElement ? a.parentElement.querySelector(':scope > .cm-submenu') : null;
                if (sm) { const f = sm.querySelector('a.cm-item:not(.cm-disabled)'); if (f) { e.preventDefault(); f.focus(); } }
                return;
            }
            if (e.key === 'ArrowLeft') {
                const a = document.activeElement;
                const inSub = a && a.closest ? a.closest('.cm-submenu') : null;
                if (inSub) { const pa = inSub.closest('.cm-li').querySelector(':scope > a.cm-item'); if (pa) { e.preventDefault(); pa.focus(); } }
                return;
            }
        });
        window.addEventListener('scroll', () => this.cmClose(), true);
        window.addEventListener('resize', () => this.cmClose());
        document.addEventListener('cms:swapped', () => this.cmClose());
    },

    // Whole-row navigation: a datatable row carrying data-dt-href navigates on a plain
    // left-click — the email-client "click anywhere on the row to open it" pattern. A
    // right-click opens the cm context menu instead (separate event); a click on a real
    // interactive element (link/button/field/label) or inside the floating menu is left
    // alone, and a modified click is ignored (no <a>, so no new-tab — a deliberate trade
    // for the whole-row target). Rows are tabindex=0 so Enter/Space open them — the
    // accessible equivalent of the click, since the subject cell no longer owns a link.
    initRowNav() {
        const go = (row) => {
            const h = row.getAttribute('data-dt-href');
            if (!h) return;
            // Route through the boost shim (partial .cms-wrap swap + shared history) when it's
            // active; fall back to a full browser navigation otherwise (no-JS boost guard or an
            // unsupported browser, where window.cmsBoost is never defined). A row-open is a view
            // change, so no dt-region target — the server answers with a full content swap.
            if (window.cmsBoost && window.cmsBoost.navigate) { window.cmsBoost.navigate(h); }
            else { location.href = h; }
        };
        // CAPTURE phase: this runs BEFORE the bubble-phase context-menu handler that calls
        // cmClose() (which nulls this._cm), so the open-menu guard is reliable — a click that
        // merely dismisses an open menu must NOT also navigate (it bails here, the menu closes
        // on bubble, a second click then navigates).
        document.addEventListener('click', (e) => {
            if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
            if (this._cm) return;
            const row = e.target.closest('tr[data-dt-href]');
            if (!row) return;
            if (e.target.closest('a,button,input,select,textarea,label,.cm-menu')) return;
            go(row);
        }, true);
        document.addEventListener('keydown', (e) => {
            if (e.key !== 'Enter' && e.key !== ' ') return;
            const row = e.target.closest ? e.target.closest('tr[data-dt-href]') : null;
            if (!row || e.target !== row) return;       // only when the ROW itself holds focus
            e.preventDefault();
            go(row);
        });
    },

    // ============================================
    // INITIALIZE ALL
    // ============================================

    init() {
        this.initParticles();
        this.initScrollReveal();
        this.initFooterYear();
        this.initDeck();
        this.initTree();
        this.initContextPanels();
        this.initContextMenu();
        this.initRowNav();
    }
};

// Auto-init when DOM is ready
if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', () => Site.init());
} else {
    Site.init();
}

// Global export
window.Site = Site;
