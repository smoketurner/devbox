//! Friendly `adjective-noun` names (e.g. `calm-quilt`).
//!
//! Shared by the server (devbox names) and the on-box agent (sandbox names). The
//! caller supplies the randomness so this crate stays free of a crypto
//! dependency; names are not a security boundary, the only requirement on the
//! source is that collisions are rare.

/// Curated adjectives. Lowercase ASCII, ≤9 chars, no offensive combinations.
pub const ADJECTIVES: &[&str] = &[
    "amber", "ample", "bold", "brave", "brisk", "calm", "clever", "cosmic", "cozy", "crisp",
    "dapper", "dawn", "deft", "eager", "early", "easy", "fair", "fancy", "fleet", "fluffy", "fond",
    "frosty", "fuzzy", "gentle", "giddy", "glad", "golden", "grand", "happy", "hardy", "hazel",
    "humble", "ivory", "jolly", "keen", "kind", "lively", "lucky", "lunar", "mellow", "merry",
    "mighty", "mint", "misty", "modest", "noble", "olive", "perky", "placid", "plucky", "polite",
    "proud", "quick", "quiet", "rapid", "regal", "ripe", "rosy", "royal", "ruddy", "rustic",
    "sage", "sandy", "sharp", "shiny", "silent", "silky", "snappy", "snowy", "solar", "spry",
    "stark", "steady", "sunny", "swift", "tidy", "trusty", "vivid", "warm", "witty", "zesty",
    "azure", "balmy", "blithe", "bonny", "breezy", "bright", "chief", "civic", "clear", "comfy",
    "dandy", "dewy", "dreamy", "elder", "epic", "fervid", "festive", "frank", "free",
];

/// Curated nouns. Lowercase ASCII, ≤9 chars.
pub const NOUNS: &[&str] = &[
    "acorn", "amber", "anchor", "arbor", "aspen", "badger", "basin", "beacon", "birch", "bison",
    "brook", "canyon", "cedar", "cobalt", "comet", "coral", "cove", "crane", "delta", "ember",
    "falcon", "fern", "finch", "fjord", "forest", "fox", "garnet", "geyser", "glade", "harbor",
    "hazel", "heron", "indigo", "iris", "jade", "kelp", "lagoon", "lark", "ledge", "lily", "lotus",
    "lynx", "maple", "marsh", "meadow", "mesa", "mist", "moss", "nebula", "nimbus", "oak", "ocean",
    "onyx", "opal", "orchid", "osprey", "otter", "pebble", "petal", "pine", "plume", "quartz",
    "quilt", "raven", "reef", "ridge", "river", "robin", "sage", "salmon", "sequoia", "shore",
    "slate", "sparrow", "spruce", "summit", "thicket", "tundra", "vale", "willow", "badge",
    "bramble", "breeze", "cliff", "clover", "creek", "dune", "fox", "gale", "grove", "harvest",
    "hollow", "isle", "lake", "moor", "nook", "pond", "spring", "thorn", "vista",
];

/// Pick a word from `words` using `raw` as the source of randomness. The modulo
/// bias is negligible at these list sizes.
fn pick(words: &[&'static str], raw: u16) -> &'static str {
    let idx = usize::from(raw).checked_rem(words.len()).unwrap_or(0);
    words.get(idx).copied().unwrap_or("box")
}

/// Build one `adjective-noun` name from four random bytes.
#[must_use]
pub fn name_from_seed(seed: [u8; 4]) -> String {
    let [a, b, c, d] = seed;
    let adj = pick(ADJECTIVES, u16::from_be_bytes([a, b]));
    let noun = pick(NOUNS, u16::from_be_bytes([c, d]));
    format!("{adj}-{noun}")
}

#[cfg(test)]
mod tests {
    use super::{ADJECTIVES, NOUNS, name_from_seed};
    use crate::is_valid_devbox_name;

    #[test]
    fn seeded_names_are_valid_devbox_names() {
        for n in 0..=u16::MAX {
            let [a, b] = n.to_be_bytes();
            let name = name_from_seed([a, b, b, a]);
            assert!(
                is_valid_devbox_name(&name),
                "generated name is not valid: {name}"
            );
            assert!(name.contains('-'), "expected adjective-noun: {name}");
        }
    }

    #[test]
    fn wordlists_contain_only_valid_fragments() {
        for word in ADJECTIVES.iter().chain(NOUNS) {
            assert!(is_valid_devbox_name(word), "invalid wordlist entry: {word}");
            assert!(word.len() <= 9, "wordlist entry too long: {word}");
        }
    }
}
