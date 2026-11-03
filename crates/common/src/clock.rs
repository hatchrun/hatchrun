// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the time in seconds since the Unix epoch, or 0 when the
/// system clock is set before it.
///
/// Not built for `wasm32`, where `SystemTime::now` panics.
#[must_use]
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::now_secs;

    #[test]
    fn is_past_2026() {
        assert!(now_secs() > 1_767_225_600);
    }
}
