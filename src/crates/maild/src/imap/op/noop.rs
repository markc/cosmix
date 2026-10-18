// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Mark Constable <mc@mixos.dev>

//! `NOOP` — legal in any state. In Selected state (Phase 2+) NOOP
//! returns untagged status updates; Phase 1 has no Selected state so
//! the response is just `OK NOOP completed`.

use crate::imap::response::{Status, tagged};

pub fn handle(tag: &str) -> String {
    tagged(tag, Status::Ok, None, "NOOP completed")
}
