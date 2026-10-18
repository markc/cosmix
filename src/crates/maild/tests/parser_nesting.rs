//! Direct dependency tests: deliberately bypass maild's MIME preflight.
use mail_parser::{Message, MessageParser, PartType};

fn nesting(message: &Message<'_>) -> (usize, usize) {
    let mut pending = vec![(message, 0)];
    let (mut deepest, mut embedded) = (0, 0);
    while let Some((message, depth)) = pending.pop() {
        deepest = deepest.max(depth);
        for part in &message.parts {
            if let PartType::Message(inner) = &part.body {
                embedded += 1;
                pending.push((inner, depth + 1));
            }
        }
    }
    (deepest, embedded)
}

fn plain(levels: usize) -> String {
    format!(
        "{}Subject: leaf\r\nContent-Type: text/plain\r\n\r\nhello",
        "Content-Type: message/rfc822\r\n\r\n".repeat(levels)
    )
}

fn on_default_sized_stack(work: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name("parser-cap-2mib".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(work)
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn direct_parser_caps_and_drops_one_hundred_thousand_plain_messages() {
    on_default_sized_stack(|| {
        let raw = plain(100_000);
        let message = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(nesting(&message), (64, 64));
        drop(message); // Recursive drop must finish on this 2 MiB stack.
    });
}

#[test]
fn direct_parser_shares_cap_across_encoded_and_plain_messages() {
    use base64::Engine;
    on_default_sized_stack(|| {
        for outer_plain in [0, 60] {
            let raw = format!(
                "{}Content-Type: message/rfc822\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
                "Content-Type: message/rfc822\r\n\r\n".repeat(outer_plain),
                base64::engine::general_purpose::STANDARD.encode(plain(100_000))
            );
            let message = MessageParser::default().parse(&raw).unwrap();
            assert_eq!(nesting(&message), (64, 64));
            drop(message);
        }
    });
}

#[test]
fn direct_parser_preserves_ten_embedded_messages() {
    on_default_sized_stack(|| {
        let raw = plain(10);
        let message = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(nesting(&message), (10, 10));
        drop(message);
    });
}
