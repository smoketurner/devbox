//! Friendly `adjective-noun` name generation for devboxes.
//!
//! Every box gets a memorable handle (e.g. `calm-quilt`) the moment the
//! reconciler creates its record, the way Claude names sessions. The name is
//! shown in the UI and CLI and doubles as a selector for `ssh`/`release`/
//! `status`, so it must be unique across non-terminated boxes.
//!
//! Randomness comes from `aws_lc_rs::rand` (the workspace crypto backend), so no
//! extra dependency is pulled in. Names are not a security boundary; the only
//! requirement on the random source is that collisions are rare.

use std::collections::HashSet;

use anyhow::Result;

use devbox_common::naming::name_from_seed;

use crate::db::DocumentStore;
use crate::documents::devbox::DevboxDoc;

/// Maximum attempts before [`generate_unique_name`] gives up. With thousands of
/// `adjective-noun` combinations against a small pool this is never reached; it
/// is a backstop so the loop always terminates.
const MAX_ATTEMPTS: usize = 64;

/// Number of plain attempts before widening the space with a numeric suffix.
const SUFFIX_AFTER: usize = 8;

/// Fill a fixed-size byte buffer from the crypto RNG.
fn fill<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    aws_lc_rs::rand::fill(&mut buf)
        .map_err(|_| anyhow::anyhow!("failed to read from the random number generator"))?;
    Ok(buf)
}

/// Generate one random `adjective-noun` name (no uniqueness check).
fn random_name() -> Result<String> {
    Ok(name_from_seed(fill::<4>()?))
}

/// Generate a name that is unique across stored boxes and the `used` set.
///
/// Checks each candidate against the `name` index in `store` and the in-memory
/// `used` set (names assigned earlier in the same tick that are not yet
/// persisted). After [`SUFFIX_AFTER`] plain collisions it appends a two-digit
/// suffix to widen the space.
///
/// # Errors
///
/// Returns an error if the RNG fails, the store lookup fails, or no unique name
/// is found within [`MAX_ATTEMPTS`] (effectively unreachable).
pub async fn generate_unique_name(store: &DocumentStore, used: &HashSet<String>) -> Result<String> {
    for attempt in 0..MAX_ATTEMPTS {
        let mut candidate = random_name()?;
        if attempt >= SUFFIX_AFTER {
            let [n] = fill::<1>()?;
            candidate = format!("{candidate}-{:02}", n.checked_rem(100).unwrap_or(0));
        }
        if used.contains(&candidate) {
            continue;
        }
        if store
            .find_one::<DevboxDoc>("name", &candidate)
            .await?
            .is_none()
        {
            return Ok(candidate);
        }
    }
    anyhow::bail!("could not generate a unique devbox name after {MAX_ATTEMPTS} attempts")
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use devbox_common::is_valid_devbox_name;

    #[test]
    fn random_name_is_a_valid_devbox_name() {
        for _ in 0..256 {
            let name = random_name().unwrap();
            assert!(
                is_valid_devbox_name(&name),
                "generated name is not valid: {name}"
            );
            assert!(name.contains('-'), "expected adjective-noun: {name}");
        }
    }
}
