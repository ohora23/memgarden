//! Temporal handling (CE-8, PR B6).
//!
//! Two sides that deliberately do not share code:
//! * [`parse`] — retain-side. Turns an ISO string or a relative expression in
//!   a *fact* into an absolute instant, relative to the retain job's
//!   `event_date`.
//! * [`query`] — recall-side. Turns a relative expression in a *query* into a
//!   `[start, end]` window, relative to now — or into the deliberate
//!   "recognized but unconstrainable" third state.

pub mod parse;
pub mod query;
