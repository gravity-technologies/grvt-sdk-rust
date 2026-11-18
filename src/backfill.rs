//! Read available history without claiming complete private-event recovery.
#[cfg(feature = "client")]
use crate::{pagination::pages, Result};
use crate::{pagination::Page, GapRange};

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BackfillPage<P> {
    pub page: P,
    pub gap: GapRange,
    pub history_exhausted: bool,
}
impl<P> BackfillPage<P> {
    pub fn continuity(&self) -> &'static str {
        "unproven"
    }
}

/// One page of history read after a gap. `history_exhausted` means this query has no next cursor; it
/// never proves that every missed event is visible, so `continuity()` stays "unproven".
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BackfillStatus {
    pub gap: GapRange,
    pub history_exhausted: bool,
}

impl BackfillStatus {
    pub fn continuity(&self) -> &'static str {
        "unproven"
    }
}

/// What one fetched page says about the gap (no I/O).
pub fn backfill_status(gap: &GapRange, page: &impl Page) -> BackfillStatus {
    BackfillStatus {
        gap: gap.clone(),
        history_exhausted: page.next_cursor().is_none_or(str::is_empty),
    }
}

/// The callback owns the REST endpoint and time window. Sequence bounds are not
/// REST cursors. Exhausted history never proves that every missed event is visible.
#[cfg(feature = "client")]
pub fn backfill_pages<P, F, Fut>(fetch: F, gap: GapRange) -> impl futures::Stream<Item = Result<BackfillPage<P>>>
where
    P: Page,
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<P>>,
{
    use futures::StreamExt;
    pages(fetch, None).map(move |result| {
        result.map(|page| {
            let status = backfill_status(&gap, &page);
            BackfillPage {
                page,
                gap: status.gap,
                history_exhausted: status.history_exhausted,
            }
        })
    })
}
