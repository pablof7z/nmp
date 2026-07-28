//! Shared opaque native token for a Rust-authored unsigned protocol draft.
//!
//! Native SDKs cannot inspect or construct this object. Protocol modules mint
//! it from validated semantic inputs; a later closed publication operation
//! may consume it without accepting raw kind/tags/author/time from Swift or
//! Kotlin. It carries no routing, signing, persistence, receipt, retry, or
//! publication claim by itself.

use std::sync::Mutex;

use nostr::UnsignedEvent;

#[derive(uniffi::Object)]
pub struct FfiProtocolDraft {
    event: Mutex<Option<UnsignedEvent>>,
}

impl FfiProtocolDraft {
    pub(crate) fn new(event: UnsignedEvent) -> Self {
        Self {
            event: Mutex::new(Some(event)),
        }
    }

    /// Consume the draft exactly once. Publication adapters must perform any
    /// synchronous validation/admission that may refuse without acceptance
    /// before calling this method.
    #[allow(dead_code)]
    pub(crate) fn take(&self) -> Option<UnsignedEvent> {
        self.event
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }

    #[cfg(test)]
    pub(crate) fn event_for_test(&self) -> Option<UnsignedEvent> {
        self.event
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}
