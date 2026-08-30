//! The bundled message corpus.
//!
//! Five shapes, each chosen because it exercises something the slice claims to
//! have validated, not because it looks like mail:
//!
//! - `newsletter` — headings, lists, rules, quotes and several links: the
//!   ordinary body, and the one that proves block projection covers the kinds.
//! - `tracked` — remote images (including a 1×1 beacon and a CSS
//!   `background-image`) plus a `javascript:` href: the remote-reference
//!   inventory and the sanitiser's URL-scheme policy, observable in the app.
//! - `thread` — nested quoting and a `<pre>` block: the deepest nesting a real
//!   reply chain produces.
//! - `plain` — no HTML at all, through the same widget.
//! - `long` — assembled here rather than shipped as a file, so a body big
//!   enough to be interesting costs three lines of source instead of a
//!   quarter-megabyte blob in the tree.

use ctk::prelude::BodySource;

/// How a fixture's body is produced.
enum Body {
    Html(&'static str),
    Plain(&'static str),
    /// Repeat a paragraph until the body is worth measuring.
    LongHtml {
        paragraphs: usize,
    },
}

pub struct Fixture {
    pub from: &'static str,
    pub address: &'static str,
    pub subject: &'static str,
    pub snippet: &'static str,
    body: Body,
}

impl Fixture {
    /// The untrusted body. Sanitisation is the caller's, at selection time.
    pub fn body(&self) -> BodySource {
        match &self.body {
            Body::Html(html) => BodySource::Html((*html).to_string()),
            Body::Plain(text) => BodySource::Plain((*text).to_string()),
            Body::LongHtml { paragraphs } => {
                let mut html = String::with_capacity(paragraphs * 220);
                html.push_str("<html><body><h1>Build log digest</h1>");
                for index in 0..*paragraphs {
                    html.push_str(&format!(
                        "<p>Step {index}: resolved dependency graph, ran the unit suite, and \
                         recorded a <a href=\"https://example.org/build/{index}\">build \
                         artifact</a>. No regressions observed at this step.</p>"
                    ));
                }
                html.push_str("</body></html>");
                BodySource::Html(html)
            }
        }
    }
}

const NEWSLETTER: &str = include_str!("../fixtures/newsletter.html");
const TRACKED: &str = include_str!("../fixtures/tracked.html");
const THREAD: &str = include_str!("../fixtures/thread.html");

const PLAIN: &str = "\
Reminder: the maintenance window is Thursday 02:00-04:00.

Nothing needs doing on your side. Services restart in dependency order and
the mesh converges on its own; if a node is still down after the window,
that is a bug and not a slow rollout.

Do not reply to this address.
";

/// The corpus, in a fixed order the store cycles through.
pub fn bodies() -> Vec<Fixture> {
    vec![
        Fixture {
            from: "Substrate Notes",
            address: "notes@example.org",
            subject: "Weekly substrate notes",
            snippet: "The property substrate landed this week, which means daemon state is now queryable…",
            body: Body::Html(NEWSLETTER),
        },
        Fixture {
            from: "Example Store",
            address: "orders@example.com",
            subject: "Your order has shipped",
            snippet: "Your order has shipped. Track it with the link below.",
            body: Body::Html(TRACKED),
        },
        Fixture {
            from: "A Colleague",
            address: "colleague@example.net",
            subject: "Re: queue drain race",
            snippet: "That matches what I saw. Merging once CI is green.",
            body: Body::Html(THREAD),
        },
        Fixture {
            from: "Operations",
            address: "ops@example.org",
            subject: "Maintenance window Thursday",
            snippet: "Reminder: the maintenance window is Thursday 02:00-04:00.",
            body: Body::Plain(PLAIN),
        },
        Fixture {
            from: "Build Robot",
            address: "builds@example.org",
            subject: "Build log digest",
            snippet: "Step 0: resolved dependency graph, ran the unit suite, and recorded a build artifact…",
            body: Body::LongHtml { paragraphs: 400 },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctk::prelude::{project_body, ProjectedBlockKind};

    #[test]
    fn every_fixture_projects_to_at_least_one_block() {
        for fixture in bodies() {
            let projection = project_body(&fixture.body().sanitize());
            assert!(
                !projection.blocks.is_empty(),
                "fixture {:?} projected to nothing",
                fixture.subject
            );
        }
    }

    #[test]
    fn the_tracked_fixture_inventories_its_remote_references() {
        let tracked = bodies()
            .into_iter()
            .find(|fixture| fixture.subject == "Your order has shipped")
            .expect("tracked fixture present");
        let body = tracked.body().sanitize();
        let refs = body.remote_refs();
        assert!(
            refs.count() >= 3,
            "expected the beacon, the logo and the CSS background, found {}",
            refs.count()
        );
        assert!(
            refs.urls().iter().any(|url| url.contains("open-beacon")),
            "the tracking beacon must be inventoried, not silently dropped"
        );
    }

    #[test]
    fn the_javascript_href_does_not_survive_sanitisation() {
        let tracked = bodies()
            .into_iter()
            .find(|fixture| fixture.subject == "Your order has shipped")
            .expect("tracked fixture present");
        let body = tracked.body().sanitize();
        let projection = project_body(&body);
        let mut resolved = 0;
        for span in projection
            .blocks
            .iter()
            .flat_map(|block| block.spans.iter())
        {
            let Some(target) = span.link_target else {
                continue;
            };
            let href = projection
                .link_target(target)
                .expect("a span's own projection resolves its target");
            resolved += 1;
            assert!(
                !href.starts_with("javascript:"),
                "an active-scheme href reached the projection: {href}"
            );
        }
        assert!(
            resolved > 0,
            "the fixture's safe tracking link must still project, or this test proves nothing"
        );
    }

    #[test]
    fn the_long_fixture_is_large_enough_to_be_worth_measuring() {
        let long = bodies()
            .into_iter()
            .find(|fixture| fixture.subject == "Build log digest")
            .expect("long fixture present");
        let projection = project_body(&long.body().sanitize());
        let paragraphs = projection
            .blocks
            .iter()
            .filter(|block| block.kind == ProjectedBlockKind::Paragraph)
            .count();
        assert!(
            paragraphs >= 200,
            "the long fixture collapsed to {paragraphs} paragraphs"
        );
    }
}
