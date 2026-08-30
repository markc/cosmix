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
    // INITIALIZE ALL
    // ============================================

    init() {
        this.initParticles();
        this.initScrollReveal();
        this.initFooterYear();
        this.initDeck();
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
