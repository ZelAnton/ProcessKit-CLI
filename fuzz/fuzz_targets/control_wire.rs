//! Fuzz the control plane's two textual wire-parsing surfaces together
//! (`src/control.rs`): the server's request-line classifier and the client's
//! response-line JSON decode.
//!
//! Seed/corpus entries are plain text, split on the *first* `\n` (never
//! present in a real one-line request, but harmless — [`classify_request`]
//! trims and compares the whole remainder, so an embedded newline just makes it
//! not match any verb, exactly as a real oversized/garbled line would not):
//! everything before it is fed to the server-side request classifier, exactly
//! as [`processkit_cli::control::classify_request`] receives the one line
//! [`AsyncBufReadExt::read_line`] hands `serve_one` (see the module's "Wire
//! protocol" doc); everything after it is fed to the client-side response
//! decode, exactly as `converse` in the same module parses the one reply line
//! back — three ways are tried, in the same order `converse` tries them: a
//! [`Snapshot`] or a [`ControlAck`] (a real client picks the type by which verb
//! it sent, information a byte-string input does not carry), and, since T-191,
//! the owned [`ErrorReply`] `converse` falls back to when neither `T` parses —
//! the shape `serve_one`'s structured `{"error": "..."}` reply takes. Never
//! expected to panic — a malformed line/reply is exactly what both real parsers
//! already treat as a routine rejection (an "unknown control request" error
//! reply on the server side, an `io::ErrorKind::InvalidData` on the client
//! side).
#![no_main]

use libfuzzer_sys::fuzz_target;
use processkit_cli::control::{ControlAck, ErrorReply, Snapshot, classify_request};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (request, response) = match text.split_once('\n') {
        Some((request, response)) => (request, response),
        None => (text, ""),
    };
    let _ = classify_request(request);
    let _ = serde_json::from_str::<Snapshot>(response.trim());
    let _ = serde_json::from_str::<ControlAck>(response.trim());
    let _ = serde_json::from_str::<ErrorReply>(response.trim());
});
