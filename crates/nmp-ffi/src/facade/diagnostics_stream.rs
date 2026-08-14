use crate::convert::{diagnostics_snapshot_to_ffi, FfiError};
use crate::types::FfiDiagnosticsSnapshot;

/// The app-facing pull-based handle to a live diagnostics stream (returned by
/// [`super::NmpEngine::observe_diagnostics`], #680). Same discipline as
/// [`super::NmpRowStream`] — await [`Self::next`], `Drop`/[`Self::cancel`] withdraw.
#[derive(uniffi::Object)]
pub struct NmpDiagnosticsStream {
    pub(super) inner: nmp::AsyncDiagnosticsSubscription,
}

#[uniffi::export]
impl NmpDiagnosticsStream {
    /// Await the next [`FfiDiagnosticsSnapshot`] — the current snapshot on the
    /// first call, a fresh one on every coverage change afterward, or `None`
    /// once the stream is withdrawn. [`FfiError::ConcurrentNext`] on an
    /// overlapping call.
    pub async fn next(&self) -> Result<Option<FfiDiagnosticsSnapshot>, FfiError> {
        match self.inner.next().await {
            Ok(Some(snapshot)) => Ok(Some(diagnostics_snapshot_to_ffi(snapshot))),
            Ok(None) => Ok(None),
            Err(_) => Err(FfiError::ConcurrentNext),
        }
    }

    /// Withdraw this diagnostics observer now, rather than waiting for `Drop`.
    /// Safe to call more than once; safe to never call at all.
    pub fn cancel(&self) {
        self.inner.cancel();
    }
}

impl Drop for NmpDiagnosticsStream {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}
