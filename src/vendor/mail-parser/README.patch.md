# mail-parser 0.11.5: bounded message nesting

Source: the published crates.io mail-parser 0.11.5 package, upstream commit
`67a53e3ded665d68d9fc66cb41d89b3ded559d0e`, registry checksum
`47785d444be4d32c1709171c6219a90f667c0ad0ffe68b4b179e794f31f4f9e8`.
Upstream sources, manifests, fixtures and Apache-2.0/MIT texts in `LICENSES/`
are retained. The registry installation marker `.cargo-ok` is omitted.

The core workspace uses `[patch.crates-io]` and excludes `vendor` from its
members, following the separate desktop workspace's convention. Desktop has
no mail-parser dependency and its lockfile is unchanged.

Only `src/parsers/message.rs` differs from upstream code. Upstream limits
encoded recursion to three but allows unbounded plain message nesting,
including inside decoded bodies. Recursive ownership and drop can overflow
any fixed thread stack. A grammar-mirroring preflight cannot be the invariant.

`MAX_NESTED_MESSAGES = 64` bounds recursive Message ownership. Before nesting,
`nesting + state_stack.len()` accounts for encoded ancestors and current plain
ancestors (multipart stack frames conservatively consume budget too). At the
cap the part becomes an ordinary binary body with `Encoding::None`; no new
Message is constructed and transfer decoding is suppressed for that part.
Encoded re-parses inherit the current combined depth plus one. The existing
three-encoded-layer limit stays in place. Multipart child IDs are flat indices,
not recursive Message ownership. Public APIs and ordinary messages are unchanged.

Direct integration tests in `crates/cosmix-maild/tests/parser_nesting.rs` bypass
all maild preflight and parse/drop on an explicitly sized 2 MiB stack: 100,000
plain levels, base64 wrapping 100,000 plain levels (also beneath 60 outer plain
levels), and an unchanged ten-level message. The cluster must run these tests;
local validation only used the authorised `cargo metadata --offline` lock update.

Exact upstream-relative patch (retain or replace with an upstream equivalent
when updating this vendor):

```diff
--- a/src/parsers/message.rs
+++ b/src/parsers/message.rs
@@ -15,6 +15,7 @@
 use super::MessageStream;
 
 const MAX_NESTED_ENCODED: usize = 3;
+const MAX_NESTED_MESSAGES: usize = 64;
 
 #[derive(Debug, PartialEq, Default)]
 enum MimeType {
@@ -109,7 +110,7 @@
     /// if no headers are found None is returned.
     ///
     pub fn parse<'x>(&self, raw_message: &'x (impl AsRef<[u8]> + ?Sized)) -> Option<Message<'x>> {
-        self.parse_(raw_message.as_ref(), MAX_NESTED_ENCODED, false)
+        self.parse_(raw_message.as_ref(), MAX_NESTED_ENCODED, false, 0)
     }
 
     /// Parses a byte slice containing the RFC5322 raw message and returns a
@@ -118,7 +119,7 @@
         &self,
         raw_message: &'x (impl AsRef<[u8]> + ?Sized),
     ) -> Option<Message<'x>> {
-        self.parse_(raw_message.as_ref(), MAX_NESTED_ENCODED, true)
+        self.parse_(raw_message.as_ref(), MAX_NESTED_ENCODED, true, 0)
     }
 
     fn parse_<'x>(
@@ -126,6 +127,7 @@
         raw_message: &'x [u8],
         depth: usize,
         skip_body: bool,
+        nesting: usize,
     ) -> Option<Message<'x>> {
         let mut stream = MessageStream::new(raw_message);
 
@@ -194,8 +196,16 @@
                 }
             }
 
+            // Bound recursive Message ownership independently of MIME spelling.
+            // Multipart stack frames conservatively consume the same budget.
+            let nesting_limited = mime_type == MimeType::Message
+                && nesting + state_stack.len() >= MAX_NESTED_MESSAGES;
+            if nesting_limited {
+                mime_type = MimeType::Other;
+            }
             let (mut encoding, decode_fnc): (Encoding, DecodeFnc<'_>) = match part_headers
                 .header_value(&HeaderName::ContentTransferEncoding)
+                .filter(|_| !nesting_limited)
             {
                 Some(HeaderValue::Text(encoding)) if encoding.eq_ignore_ascii_case("base64") => {
                     (Encoding::Base64, MessageStream::decode_base64_mime)
@@ -354,7 +364,12 @@
                 message.attachments.push(message.parts.len() as u32);
 
                 if depth != 0 {
-                    if let Some(nested_message) = self.parse_(bytes.as_ref(), depth - 1, false) {
+                    if let Some(nested_message) = self.parse_(
+                        bytes.as_ref(),
+                        depth - 1,
+                        false,
+                        nesting + state_stack.len() + 1,
+                    ) {
                         PartType::Message(Message {
                             html_body: nested_message.html_body,
                             text_body: nested_message.text_body,
```

