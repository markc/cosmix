/* Base JS - Mobile-First App Shell
 * Copyright (C) 2015-2026 Mark Constable <mc@netserva.org> (MIT License)
 */
if (typeof Base === 'undefined') {
const Base = {
    // All state in single localStorage key
    key: 'base-state',

    // Get/set persistent state
    state(updates) {
        const s = JSON.parse(localStorage.getItem(this.key) || '{}');
        if (!updates) return s;
        Object.assign(s, updates);
        localStorage.setItem(this.key, JSON.stringify(s));
        return s;
    },

    // Theme: toggle dark/light
    toggleTheme() {
        const html = document.documentElement;
        const isDark = html.classList.contains('dark');
        html.classList.replace(isDark ? 'dark' : 'light', isDark ? 'light' : 'dark');
        this.state({ theme: isDark ? 'light' : 'dark' });
        this.updateIcon();
    },

    // Update theme icon (sun/moon)
    updateIcon() {
        const btn = document.getElementById('theme-icon');
        if (!btn) return;
        const isDark = document.documentElement.classList.contains('dark');
        btn.setAttribute('aria-label', isDark ? 'Light mode' : 'Dark mode');
        const icon = btn.querySelector('[data-lucide], svg');
        if (icon && typeof lucide !== 'undefined') {
            const i = document.createElement('i');
            i.setAttribute('data-lucide', isDark ? 'sun' : 'moon');
            icon.replaceWith(i);
            lucide.createIcons({ nodes: [i] });
        } else if (!icon) {
            btn.textContent = isDark ? '\u2600\uFE0F' : '\uD83C\uDF19';
        }
    },

    // Color scheme
    setScheme(scheme) {
        const html = document.documentElement;
        ['crimson', 'stone', 'ocean', 'forest', 'sunset'].forEach(s => html.classList.remove('scheme-' + s));
        if (scheme && scheme !== 'default') html.classList.add('scheme-' + scheme);
        this.state({ scheme: scheme || 'default' });
        document.querySelectorAll('[data-scheme]').forEach(el =>
            el.classList.toggle('active', el.dataset.scheme === (scheme || 'default'))
        );
    },

    // Toast notification
    toast(msg, type = 'success', ms = 3000) {
        document.querySelector('.toast')?.remove();
        const t = document.createElement('div');
        t.className = `toast toast-${type}`;
        t.textContent = msg;
        t.setAttribute('role', 'alert');
        document.body.appendChild(t);
        setTimeout(() => { t.style.opacity = '0'; setTimeout(() => t.remove(), 300); }, ms);
    },

    // Sidebar: toggle open/close
    toggleSidebar(side) {
        const sb = document.querySelector(`.sidebar-${side}`);
        if (!sb) return;
        const opening = !sb.classList.contains('open');
        // Close all non-pinned sidebars first
        document.querySelectorAll('.sidebar.open:not(.pinned)').forEach(s => s.classList.remove('open'));
        if (opening) {
            sb.classList.add('open');
            document.body.classList.add('sidebar-open');
            this.state({ [side + 'Open']: true });
        } else {
            // Also unpin if pinned
            sb.classList.remove('open', 'pinned');
            document.body.classList.remove(side + '-pinned');
            if (!document.querySelector('.sidebar.open')) document.body.classList.remove('sidebar-open');
            this.state({ [side + 'Open']: false, [side + 'Pinned']: false });
        }
    },

    // Set theme explicitly (light/dark)
    setTheme(theme) {
        const html = document.documentElement;
        html.classList.remove('light', 'dark');
        html.classList.add(theme);
        this.state({ theme });
        this.updateIcon();
        document.querySelectorAll('[data-theme]').forEach(el =>
            el.classList.toggle('active', el.dataset.theme === theme)
        );
    },

    // Carousel transition mode (slide/fade)
    setCarouselMode(mode) {
        this.state({ carousel: mode });
        document.querySelectorAll('.sidebar').forEach(sb => {
            const track = sb.querySelector('.panel-track');
            if (!track) return;
            const panels = track.querySelectorAll('.panel');
            const side = sb.classList.contains('sidebar-left') ? 'left' : 'right';
            const idx = this.state()[side + 'Panel'] || 0;
            // Kill all transitions during mode switch
            track.style.transition = 'none';
            panels.forEach(p => p.style.transition = 'none');
            if (mode === 'fade') {
                panels.forEach((p, i) => p.classList.toggle('active', i === idx));
                track.style.transform = '';
                sb.classList.add('fade-mode');
            } else {
                track.style.transform = `translateX(-${idx * 100}%)`;
                sb.classList.remove('fade-mode');
                panels.forEach(p => p.classList.remove('active'));
            }
            // Force reflow then restore transitions
            track.offsetHeight;
            track.style.transition = '';
            panels.forEach(p => p.style.transition = '');
        });
        document.querySelectorAll('[data-carousel]').forEach(el =>
            el.classList.toggle('active', el.dataset.carousel === mode)
        );
    },

    // Sidebar width
    setSidebarWidth(px) {
        document.documentElement.style.setProperty('--sidebar-width', px + 'px');
        this.state({ sidebarWidth: px });
        const label = document.getElementById('sidebar-width-value');
        if (label) label.textContent = px;
        const slider = document.querySelector('.sidebar-width-slider');
        if (slider && parseInt(slider.value) !== px) slider.value = px;
    },

    // Panel carousel: navigate to panel index
    setPanel(side, index) {
        const sb = document.querySelector(`.sidebar-${side}`);
        if (!sb) return;
        const track = sb.querySelector('.panel-track');
        if (!track) return;
        const panels = track.querySelectorAll('.panel');
        const count = panels.length;
        if (count === 0) return;
        // Wrap around
        index = ((index % count) + count) % count;
        const isFade = sb.classList.contains('fade-mode');
        if (isFade) {
            panels.forEach((p, i) => p.classList.toggle('active', i === index));
        } else {
            track.style.transform = `translateX(-${index * 100}%)`;
        }
        // Update dots
        sb.querySelectorAll('.carousel-dot').forEach((dot, i) => {
            dot.classList.toggle('active', i === index);
        });
        this.state({ [side + 'Panel']: index });
    },

    // Sidebar: pin/unpin (desktop)
    pinSidebar(side) {
        const sb = document.querySelector(`.sidebar-${side}`);
        if (!sb) return;
        const pinning = !sb.classList.contains('pinned');
        sb.classList.toggle('pinned', pinning);
        sb.classList.toggle('open', pinning);
        document.body.classList.toggle(side + '-pinned', pinning);
        if (!pinning && !document.querySelector('.sidebar.open')) document.body.classList.remove('sidebar-open');
        this.state({ [side + 'Pinned']: pinning, [side + 'Open']: pinning });
        // Update pin icon
        const icon = sb.querySelector('.pin-toggle [data-lucide], .pin-toggle svg');
        if (icon && typeof lucide !== 'undefined') {
            const i = document.createElement('i');
            i.setAttribute('data-lucide', pinning ? 'pin-off' : 'pin');
            icon.replaceWith(i);
            lucide.createIcons({ nodes: [i] });
        }
    },

    // Close all non-pinned sidebars
    closeSidebars() {
        document.querySelectorAll('.sidebar.open:not(.pinned)').forEach(s => s.classList.remove('open'));
        if (!document.querySelector('.sidebar.pinned.open')) document.body.classList.remove('sidebar-open');
        this.state({ leftOpen: false, rightOpen: false });
    },

    // Restore state on page load
    restore() {
        const s = this.state();
        const desktop = window.innerWidth >= 1280;

        // Restore sidebar width
        if (s.sidebarWidth) this.setSidebarWidth(s.sidebarWidth);

        // Restore carousel transition mode
        const mode = s.carousel || 'slide';
        if (mode === 'fade') {
            document.querySelectorAll('.sidebar').forEach(sb => sb.classList.add('fade-mode'));
        }
        document.querySelectorAll('[data-carousel]').forEach(el =>
            el.classList.toggle('active', el.dataset.carousel === mode)
        );

        // Restore theme toggle buttons
        const theme = s.theme || (document.documentElement.classList.contains('dark') ? 'dark' : 'light');
        document.querySelectorAll('[data-theme]').forEach(el =>
            el.classList.toggle('active', el.dataset.theme === theme)
        );

        ['left', 'right'].forEach(side => {
            const sb = document.querySelector(`.sidebar-${side}`);
            if (!sb) return;
            const pinned = s[side + 'Pinned'] && desktop;
            const open = pinned || (s[side + 'Open'] && desktop);
            sb.classList.toggle('pinned', pinned);
            sb.classList.toggle('open', open);
            document.body.classList.toggle(side + '-pinned', pinned);
            if (open) document.body.classList.add('sidebar-open');
            // Set correct pin icon
            const icon = sb.querySelector('.pin-toggle [data-lucide], .pin-toggle svg');
            if (icon && pinned) icon.setAttribute('data-lucide', 'pin-off');
            // Restore panel carousel position
            const panelIndex = s[side + 'Panel'] || 0;
            if (panelIndex > 0) this.setPanel(side, panelIndex);
            else if (mode === 'fade') {
                // Ensure first panel is active in fade mode
                const panels = sb.querySelectorAll('.panel');
                if (panels[0]) panels[0].classList.add('active');
            }
        });

        // Restore tree expanded state
        const tree = document.querySelector('.tree');
        if (tree && s.treeExpanded) {
            const branches = tree.querySelectorAll('.tree-branch');
            branches.forEach((b, idx) => {
                const shouldExpand = s.treeExpanded.includes(idx);
                b.classList.toggle('collapsed', !shouldExpand);
                // Update folder icon
                const icon = b.querySelector('.tree-toggle [data-lucide="folder"], .tree-toggle [data-lucide="folder-open"]');
                if (icon) icon.setAttribute('data-lucide', shouldExpand ? 'folder-open' : 'folder');
            });
        }
    },

    // Initialize
    init() {
        this.updateIcon();
        this.restore();

        // Scheme links
        const s = this.state();
        document.querySelectorAll('[data-scheme]').forEach(el =>
            el.classList.toggle('active', el.dataset.scheme === (s.scheme || 'default'))
        );

        // Event delegation for clicks
        document.addEventListener('click', e => {
            const t = e.target;

            // Theme toggle (legacy icon button)
            if (t.closest('.theme-toggle')) { this.toggleTheme(); return; }

            // Theme buttons (Light/Dark)
            const themeBtn = t.closest('[data-theme]');
            if (themeBtn) { this.setTheme(themeBtn.dataset.theme); return; }

            // Carousel mode buttons (Slide/Fade)
            const carouselBtn = t.closest('[data-carousel]');
            if (carouselBtn) { this.setCarouselMode(carouselBtn.dataset.carousel); return; }

            // Scheme selector
            const scheme = t.closest('[data-scheme]');
            if (scheme) { e.preventDefault(); this.setScheme(scheme.dataset.scheme); return; }

            // Sidebar toggle
            const menuBtn = t.closest('.menu-toggle[data-sidebar]');
            if (menuBtn) { this.toggleSidebar(menuBtn.dataset.sidebar); return; }

            // Pin toggle
            const pinBtn = t.closest('.pin-toggle[data-sidebar]');
            if (pinBtn) { this.pinSidebar(pinBtn.dataset.sidebar); return; }

            // Carousel chevron
            const chevron = t.closest('.carousel-chevron[data-sidebar]');
            if (chevron) {
                const side = chevron.dataset.sidebar;
                const s = this.state();
                const cur = s[side + 'Panel'] || 0;
                this.setPanel(side, chevron.dataset.dir === 'prev' ? cur - 1 : cur + 1);
                return;
            }

            // Carousel dot
            const dot = t.closest('.carousel-dot[data-sidebar]');
            if (dot) {
                this.setPanel(dot.dataset.sidebar, parseInt(dot.dataset.panel, 10));
                return;
            }

            // Overlay click
            if (t.closest('.overlay')) { this.closeSidebars(); return; }

            // Tree toggle (expand/collapse)
            const treeToggle = t.closest('.tree-toggle');
            if (treeToggle) {
                const branch = treeToggle.closest('.tree-branch');
                if (!branch) return;
                const collapsed = branch.classList.toggle('collapsed');
                // Swap folder icon
                const icon = treeToggle.querySelector('[data-lucide="folder"], [data-lucide="folder-open"], svg');
                if (icon && typeof lucide !== 'undefined') {
                    const i = document.createElement('i');
                    i.setAttribute('data-lucide', collapsed ? 'folder' : 'folder-open');
                    icon.replaceWith(i);
                    lucide.createIcons({ nodes: [i] });
                }
                // Persist expanded state
                const tree = branch.closest('.tree');
                if (tree) {
                    const branches = tree.querySelectorAll('.tree-branch');
                    const expanded = [];
                    branches.forEach((b, idx) => { if (!b.classList.contains('collapsed')) expanded.push(idx); });
                    this.state({ treeExpanded: expanded });
                }
                return;
            }

            // Sidebar group toggle (collapsible)
            const groupTitle = t.closest('.sidebar-group-title');
            if (groupTitle) {
                const group = groupTitle.closest('.sidebar-group');
                group?.classList.toggle('collapsed');
                return;
            }

            // Dropdown toggle
            const dropToggle = t.closest('.dropdown-toggle');
            if (dropToggle) {
                e.preventDefault();
                e.stopPropagation();
                const dd = dropToggle.closest('.dropdown');
                document.querySelectorAll('.dropdown.open').forEach(d => d !== dd && d.classList.remove('open'));
                dd?.classList.toggle('open');
                return;
            }

            // Close dropdowns on outside click
            if (!t.closest('.dropdown')) {
                document.querySelectorAll('.dropdown.open').forEach(d => d.classList.remove('open'));
            }

            // Close non-pinned sidebars on outside click
            if (!t.closest('.sidebar') && !t.closest('.menu-toggle')) {
                document.querySelectorAll('.sidebar.open:not(.pinned)').forEach(sb => sb.classList.remove('open'));
                if (!document.querySelector('.sidebar.pinned.open')) document.body.classList.remove('sidebar-open');
            }
        });

        // Escape key closes menus
        document.addEventListener('keydown', e => {
            if (e.key === 'Escape') {
                document.querySelectorAll('.dropdown.open').forEach(d => d.classList.remove('open'));
                this.closeSidebars();
            }
        });

        // System theme change
        matchMedia('(prefers-color-scheme:dark)').addEventListener('change', e => {
            if (!this.state().theme) {
                document.documentElement.classList.replace(e.matches ? 'light' : 'dark', e.matches ? 'dark' : 'light');
                this.updateIcon();
            }
        });

        // Responsive: hide pinned sidebars when viewport shrinks to mobile
        matchMedia('(min-width: 1280px)').addEventListener('change', e => {
            if (!e.matches) {
                // Viewport went below desktop - close all sidebars
                document.querySelectorAll('.sidebar.open').forEach(sb => {
                    sb.classList.remove('open', 'pinned');
                });
                document.body.classList.remove('left-pinned', 'right-pinned', 'sidebar-open');
            } else {
                // Viewport went to desktop - restore pinned state
                this.restore();
            }
        });

        // Sidebar width slider
        const slider = document.querySelector('.sidebar-width-slider');
        if (slider) {
            slider.addEventListener('input', e => this.setSidebarWidth(parseInt(e.target.value)));
        }

        // Scroll detection: seamless topnav/sidebar header effect
        const onScroll = () => document.body.classList.toggle('scrolled', window.scrollY > 0);
        window.addEventListener('scroll', onScroll, { passive: true });
        onScroll();

        // Lucide icons
        if (typeof lucide !== 'undefined') lucide.createIcons();

        // Remove preload class to enable transitions (after state restored)
        requestAnimationFrame(() => document.documentElement.classList.remove('preload'));
    }
};

// Auto-init
document.readyState === 'loading'
    ? document.addEventListener('DOMContentLoaded', () => Base.init())
    : Base.init();

// Global exports
window.Base = Base;
window.showToast = (m, t) => Base.toast(m, t);
window.toggleTheme = () => Base.toggleTheme();
}
