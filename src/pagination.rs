//! Cursor pages. `PageCursor` is the no-I/O rule (stop at an empty cursor, refuse a repeated one); with
//! the client, `pages` fetches complete pages only when the stream is polled.
use crate::{Error, Result};
use std::collections::HashSet;

/// A response carrying the server's next cursor. Generated responses implement this.
pub trait Page {
    fn next_cursor(&self) -> Option<&str>;
}

/// What to do next in a cursor walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageStep {
    /// Fetch a page with this cursor (`None` for the first page).
    Fetch(Option<String>),
    /// The last page had no next cursor.
    Done,
}

/// Walks cursors without I/O: ask `next` for the cursor to send, then hand back each page.
/// Cursors do not guarantee a stable server snapshot.
#[derive(Debug, Clone, Default)]
pub struct PageCursor {
    cursor: Option<String>,
    seen: HashSet<String>,
    done: bool,
}

impl PageCursor {
    /// Start at `cursor` (`None` for the first page).
    pub fn new(cursor: Option<String>) -> Self {
        PageCursor {
            cursor,
            seen: HashSet::new(),
            done: false,
        }
    }

    /// What to fetch next, or `PaginationCycle` when a cursor repeats.
    pub fn next_page(&mut self) -> Result<PageStep> {
        if self.done {
            return Ok(PageStep::Done);
        }
        if let Some(c) = self.cursor.as_ref().filter(|c| !c.is_empty()) {
            if !self.seen.insert(c.clone()) {
                return Err(Error::PaginationCycle { cursor: c.clone() });
            }
        }
        Ok(PageStep::Fetch(self.cursor.clone()))
    }

    /// Record a page's next cursor. Empty pages can still continue.
    pub fn advance(&mut self, page: &impl Page) {
        self.cursor = page.next_cursor().filter(|c| !c.is_empty()).map(str::to_owned);
        self.done = self.cursor.is_none();
    }

    pub fn done(&self) -> bool {
        self.done
    }
}

/// The callback owns filters and timeout. Dropping the stream stops further calls.
/// Empty pages can still continue. Cursors do not guarantee a stable server snapshot.
#[cfg(feature = "client")]
pub fn pages<P, F, Fut>(fetch: F, cursor: Option<String>) -> impl futures::Stream<Item = Result<P>>
where
    P: Page,
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<P>>,
{
    futures::stream::try_unfold((fetch, PageCursor::new(cursor)), |(mut fetch, mut walk)| async move {
        let PageStep::Fetch(cursor) = walk.next_page()? else {
            return Ok(None);
        };
        let page = fetch(cursor).await?;
        walk.advance(&page);
        Ok(Some((page, (fetch, walk))))
    })
}
