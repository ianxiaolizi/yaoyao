//! The process's one reading of the wall clock.
//!
//! Every decision that depends on "now" takes it as a parameter. This module is
//! the one place the binary reads the clock, so the app edge is the only code
//! that has to be trusted with it: the kernel and every adapter judge an input
//! against a time they were handed, which is what makes a replay from the log
//! and a supplied instant reproduce the same decision.
//!
//! Not a clock port. The app edge is the only consumer, and a port with one
//! implementation and no test that substitutes another would add a seam nobody
//! crosses.

use chrono::{DateTime, Utc};

/// The current instant, in UTC.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}
