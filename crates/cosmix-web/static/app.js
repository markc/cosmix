// Cosmix app.js — HTMX glue for DCS shell
// Handles nav active state after HTMX page swaps

document.addEventListener('DOMContentLoaded', () => {
    // Track active nav link after HTMX content swaps
    document.body.addEventListener('htmx:afterSwap', (e) => {
        if (e.detail.target.id === 'content') {
            const path = e.detail.requestConfig.path;
            document.querySelectorAll('#panel-nav a').forEach(a => {
                a.classList.toggle('active',
                    a.getAttribute('hx-get') === path);
            });
        }
    });
});
