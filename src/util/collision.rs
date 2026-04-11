//! Pure name-collision resolver. Used by the tmux actor before
//! creating a new session: if the desired display name already
//! exists in the live session list, append " 2", " 3", etc. until
//! we find an unused one. Agent-deck convention: trailing space +
//! number, so "Bosun" → "Bosun 2" (not "Bosun-2" or "Bosun(2)").
//!
//! Pure function, no tmux, no I/O. Unit-tested.

/// If `desired` doesn't collide with any entry in `existing`, return it
/// as-is. Otherwise find the smallest `n >= 2` such that
/// `"{desired} {n}"` is free.
///
/// Safety rail: gives up after 9999 tries and returns `desired` as a
/// last resort (at which point the user has bigger problems).
pub fn resolve_name_collision(desired: &str, existing: &[String]) -> String {
    if !existing.iter().any(|e| e == desired) {
        return desired.to_string();
    }
    for n in 2..10000 {
        let candidate = format!("{} {}", desired, n);
        if !existing.iter().any(|e| e == &candidate) {
            return candidate;
        }
    }
    desired.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_collision_returns_desired() {
        assert_eq!(resolve_name_collision("Bosun", &[]), "Bosun");
        assert_eq!(
            resolve_name_collision("Bosun", &["other".to_string()]),
            "Bosun"
        );
    }

    #[test]
    fn first_collision_becomes_2() {
        assert_eq!(
            resolve_name_collision("Bosun", &["Bosun".to_string()]),
            "Bosun 2"
        );
    }

    #[test]
    fn second_collision_becomes_3() {
        let existing = vec!["Bosun".to_string(), "Bosun 2".to_string()];
        assert_eq!(resolve_name_collision("Bosun", &existing), "Bosun 3");
    }

    #[test]
    fn collision_skips_numbered_gaps() {
        let existing = vec!["Bosun".to_string(), "Bosun 3".to_string()];
        assert_eq!(resolve_name_collision("Bosun", &existing), "Bosun 2");
    }

    #[test]
    fn name_with_space_still_gets_suffixed() {
        let existing = vec!["My Rocket Fox".to_string()];
        assert_eq!(
            resolve_name_collision("My Rocket Fox", &existing),
            "My Rocket Fox 2"
        );
    }

    #[test]
    fn does_not_collide_on_substring() {
        // "Bosun" exists; creating "Bosun Planet" should NOT
        // collide with "Bosun" because we match whole strings.
        let existing = vec!["Bosun".to_string()];
        assert_eq!(
            resolve_name_collision("Bosun Planet", &existing),
            "Bosun Planet"
        );
    }
}
