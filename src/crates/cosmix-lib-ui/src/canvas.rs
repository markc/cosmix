/// CSS for the pan/zoom canvas container.
/// Used by DOT graph viewer, image viewer, and any future zoomable content.
pub const CANVAS_CSS: &str = r#"
.pan-canvas {
    width: 100%; height: 100vh;
    overflow: hidden;
    position: relative;
    cursor: grab;
}
.pan-canvas:active { cursor: grabbing; }
.pan-content {
    transform-origin: 0 0;
    position: absolute;
    top: 0; left: 0;
}
.pan-content svg { display: block; }
.pan-content img { display: block; max-width: none; max-height: none; }
.pan-controls {
    position: fixed;
    bottom: 8px; right: 12px;
    font-size: 11px;
    color: #6b7280;
    background: rgba(255,255,255,0.85);
    padding: 3px 10px;
    border-radius: 4px;
    pointer-events: none;
    font-family: system-ui, sans-serif;
}
"#;

/// JavaScript for pan/zoom behaviour.
/// Expects a `#pan-content` element inside a `.pan-canvas` parent.
/// Scroll = zoom (centered on cursor), drag = pan, double-click = reset.
pub const CANVAS_JS: &str = r#"
(function() {
    let scale = 1, panX = 0, panY = 0;
    let dragging = false, startX = 0, startY = 0, startPanX = 0, startPanY = 0;
    const el = document.getElementById('pan-content');
    if (!el) return;
    const canvas = el.parentElement;

    function apply() {
        el.style.transform = 'translate(' + panX + 'px,' + panY + 'px) scale(' + scale + ')';
    }

    function centerView() {
        const child = el.querySelector('svg') || el.querySelector('img');
        if (!child) return;
        const sw = child.naturalWidth || (child.getBoundingClientRect().width / scale);
        const sh = child.naturalHeight || (child.getBoundingClientRect().height / scale);
        if (!sw || !sh) return;
        const cw = canvas.clientWidth;
        const ch = canvas.clientHeight;
        scale = Math.min(cw / sw, ch / sh, 1) * 0.9;
        panX = (cw - sw * scale) / 2;
        panY = (ch - sh * scale) / 2;
        apply();
    }

    function initCenter() {
        const img = el.querySelector('img');
        if (img) {
            if (img.complete && img.naturalWidth > 0) {
                setTimeout(centerView, 50);
            } else {
                img.addEventListener('load', centerView);
            }
        } else {
            setTimeout(centerView, 50);
        }
    }
    initCenter();

    canvas.addEventListener('wheel', function(e) {
        e.preventDefault();
        const rect = canvas.getBoundingClientRect();
        const mx = e.clientX - rect.left;
        const my = e.clientY - rect.top;
        const factor = e.deltaY < 0 ? 1.15 : 1 / 1.15;
        const newScale = Math.max(0.1, Math.min(10, scale * factor));
        panX = mx - (mx - panX) * (newScale / scale);
        panY = my - (my - panY) * (newScale / scale);
        scale = newScale;
        apply();
    }, {passive: false});

    canvas.addEventListener('mousedown', function(e) {
        if (e.button !== 0) return;
        dragging = true;
        startX = e.clientX; startY = e.clientY;
        startPanX = panX; startPanY = panY;
    });
    window.addEventListener('mousemove', function(e) {
        if (!dragging) return;
        panX = startPanX + (e.clientX - startX);
        panY = startPanY + (e.clientY - startY);
        apply();
    });
    window.addEventListener('mouseup', function() { dragging = false; });

    canvas.addEventListener('dblclick', function() { centerView(); });
})();
"#;

pub const CANVAS_CONTROLS_TEXT: &str = "Scroll: zoom | Drag: pan | Double-click: reset";
