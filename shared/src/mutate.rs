//! Malformed variants of a valid input, for tests that every parser of user or tool input
//! fails cleanly instead of panicking. Deterministic, so a failure reproduces.

/// Characters that break structure in JSON and TOML.
const STRUCTURAL: &[u8] = b"{}[]\",:=.#\\\n\t 0-ze\x00\x7f";

/// Every prefix of `text` that ends on a character boundary, then `edits` random variants:
/// a byte replaced by a structural character, a byte removed, or a span repeated. The same
/// `seed` always gives the same variants.
pub fn mutations(text: &str, seed: u64, edits: usize) -> Vec<String> {
    let mut variants: Vec<String> = text
        .char_indices()
        .map(|(end, _)| text[..end].to_owned())
        .collect();
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return variants;
    }
    let mut random = XorShift(seed | 1);
    for _ in 0..edits {
        let mut edited = bytes.to_vec();
        let at = random.below(edited.len());
        match random.below(3) {
            0 => edited[at] = STRUCTURAL[random.below(STRUCTURAL.len())],
            1 => {
                edited.remove(at);
            }
            _ => {
                let end = (at + 1 + random.below(16)).min(edited.len());
                let span = edited[at..end].to_vec();
                edited.splice(at..at, span);
            }
        }
        variants.push(String::from_utf8_lossy(&edited).into_owned());
    }
    variants
}

/// xorshift64: tiny, deterministic, and good enough to pick positions.
struct XorShift(u64);

impl XorShift {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        // The remainder is below `bound`, so it fits back into a usize.
        (self.0 % bound as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mutations_cover_every_prefix_and_repeat_for_a_seed() {
        let variants = mutations("aé{", 7, 20);
        assert_eq!(&variants[..3], ["", "a", "aé"]);
        assert_eq!(variants.len(), 23);
        assert_eq!(variants, mutations("aé{", 7, 20));
        assert_ne!(variants, mutations("aé{", 8, 20));
    }
}
