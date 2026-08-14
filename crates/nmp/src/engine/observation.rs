use nmp_grammar::LiveQuery;

use super::Engine;
use crate::error::EngineError;
use crate::runtime::{Handle, HistoryHandle, HistoryReceiver, QueryHandle, RowsReceiver};
use crate::subscription::{AsyncSubscription, Subscription, Window};

impl Engine {
    /// Noun 1: open a live query (#485). `window: None` ⇒ the unbounded delta
    /// observation (semantics unchanged from the pre-#485 `observe`).
    /// `Some(`[`Window::Expandable`]`)` ⇒ a bounded newest-first snapshot
    /// observation, growable via [`Subscription::request_rows`]. Delivery mode
    /// is DERIVED from boundedness (see [`crate::Subscription`]'s doc), never a
    /// separate knob. The returned [`Subscription`] withdraws itself on `Drop`.
    ///
    /// Windowed validation (typed on [`EngineError`], caught here BEFORE the
    /// engine is touched):
    /// - `initial > max` ⇒ [`EngineError::WindowInitialExceedsMax`].
    /// - a selection that already carries a NIP-01 `limit` ⇒
    ///   [`EngineError::WindowSelectionHasLimit`] (a window and a `limit` would
    ///   be two competing owners of row membership).
    ///
    /// Zero-sized windows are unrepresentable: [`Window::Expandable`] uses
    /// `NonZeroUsize`.
    pub fn observe(
        &self,
        query: LiveQuery,
        window: Option<Window>,
    ) -> Result<Subscription, EngineError> {
        self.subscribe_observation(query, window, Subscription::new, Subscription::new_windowed)
    }

    /// The pull-based async twin of [`Self::observe`] (#680): returns an
    /// [`AsyncSubscription`] whose `next()` is awaited rather than blocked on.
    /// Identical demand, validation, windowing, and withdrawal semantics — only
    /// the delivery wakeup differs (a waker, not a dedicated OS thread). This is
    /// what the FFI/SDK observation handles are built on, so opening one costs
    /// no native thread. Doc-hidden: it is the FFI/SDK delivery mechanism, not
    /// the documented direct-Rust product noun (which is blocking [`Self::observe`]).
    #[doc(hidden)]
    pub fn observe_async(
        &self,
        query: LiveQuery,
        window: Option<Window>,
    ) -> Result<AsyncSubscription, EngineError> {
        self.subscribe_observation(
            query,
            window,
            AsyncSubscription::new,
            AsyncSubscription::new_windowed,
        )
    }

    /// Shared validation + engine-subscribe for both the blocking and async
    /// observation surfaces (#680). The two closures select which wrapper
    /// (blocking `Subscription` vs `AsyncSubscription`) receives the raw engine
    /// handle + receiver, so the window/limit validation lives in exactly one
    /// place.
    fn subscribe_observation<T>(
        &self,
        query: LiveQuery,
        window: Option<Window>,
        unbounded: impl FnOnce(Handle, QueryHandle, RowsReceiver) -> T,
        windowed: impl FnOnce(Handle, HistoryHandle, HistoryReceiver) -> T,
    ) -> Result<T, EngineError> {
        match window {
            None => self
                .with_handle(|handle| {
                    handle
                        .subscribe(query)
                        .map(|(query_handle, rows)| unbounded(handle.clone(), query_handle, rows))
                })?
                .map_err(EngineError::from_observe_error),
            Some(Window::Expandable { initial, max }) => {
                if initial > max {
                    return Err(EngineError::WindowInitialExceedsMax {
                        initial: initial.get(),
                        max: max.get(),
                    });
                }
                if query
                    .branches()
                    .iter()
                    .any(|branch| branch.selection.limit.is_some())
                {
                    return Err(EngineError::WindowSelectionHasLimit);
                }
                // A window and an aggregate result limit are two competing
                // owners of the same row-membership count. Refuse before the
                // engine is touched, exactly as a branch selection limit is.
                if query.aggregate_result_limit().is_some() {
                    return Err(EngineError::WindowAggregateResultLimit);
                }
                let history_query = crate::core::HistoryQuery::new(query, initial.get(), max.get());
                self.with_handle(|handle| {
                    handle
                        .subscribe_history(history_query)
                        .map(|(history_handle, batches)| {
                            windowed(handle.clone(), history_handle, batches)
                        })
                })?
                .map_err(EngineError::from_observe_error)
            }
        }
    }
}
