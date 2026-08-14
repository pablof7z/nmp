use crate::convert::{sign_event_failure, signed_event_to_ffi};
use crate::types::{FfiSignEventFailure, FfiSignedEvent};

/// Scoped one-shot sign-only handle (#680). It owns no signer registration and
/// cannot affect accepted durable writes. Await [`Self::signed`] once for the
/// verified event (or a typed failure); [`Self::cancel`] cancels only this
/// signer operation.
#[derive(uniffi::Object)]
pub struct NmpSignEventHandle {
    pub(super) cancel: nmp::SignEventCancel,
    pub(super) result: nmp::AsyncFifoReceiver<Result<nmp::Event, nmp::SignEventError>>,
}

#[uniffi::export]
impl NmpSignEventHandle {
    /// Await the one-shot outcome: the fully-verified signed event, or a typed
    /// [`FfiSignEventFailure`]. This is one-shot — a second await (sequential or
    /// concurrent) returns [`FfiSignEventFailure::AlreadyConsumed`], because the
    /// single result was already delivered to the first await.
    pub async fn signed(&self) -> Result<FfiSignedEvent, FfiSignEventFailure> {
        match self.result.next().await {
            Ok(Some(Ok(event))) => Ok(signed_event_to_ffi(event)),
            Ok(Some(Err(error))) => Err(sign_event_failure(error)),
            Ok(None) | Err(_) => Err(FfiSignEventFailure::AlreadyConsumed),
        }
    }

    /// Cancel this sign-only operation. Idempotent; safe after completion.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for NmpSignEventHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
