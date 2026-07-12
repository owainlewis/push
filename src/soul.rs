//! User-owned assistant identity injected into every backend run.

use std::path::Path;

use anyhow::{Context, Result};

const SOUL_FILE: &str = "SOUL.md";
const INVARIANTS: &str = "The assistant identity file is owned by the user. Do not modify SOUL.md.";

/// Reads `SOUL.md` from `dir` and appends gateway-owned invariants in memory.
///
/// A missing or empty file produces only the invariants. Other read failures
/// are returned to the gateway. Push never creates or changes the file.
pub fn load(dir: &str) -> Result<String> {
    let path = Path::new(dir).join(SOUL_FILE);
    let soul = match std::fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    }
    .map(|contents| contents.trim().to_string())
    .filter(|contents| !contents.is_empty());

    Ok(match soul {
        Some(soul) => format!("{soul}\n\n{INVARIANTS}"),
        None => INVARIANTS.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    #[test]
    fn loads_soul_and_appends_invariants_without_rewriting_file() {
        let dir = temp_dir("soul");
        let path = dir.join(SOUL_FILE);
        let original = "Be calm, direct, and curious.\n";
        std::fs::write(&path, original).unwrap();

        let instructions = load(dir.to_str().unwrap()).unwrap();

        assert!(instructions.starts_with("Be calm, direct, and curious."));
        assert!(instructions.ends_with(INVARIANTS));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_soul_has_predictable_gateway_invariants() {
        let dir = temp_dir("missing-soul");

        assert_eq!(load(dir.to_str().unwrap()).unwrap(), INVARIANTS);
        assert!(!dir.join(SOUL_FILE).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ignores_legacy_context_files() {
        let dir = temp_dir("legacy-context");
        std::fs::write(dir.join("User.md"), "legacy user context").unwrap();
        std::fs::write(dir.join("Memory.md"), "legacy memory").unwrap();

        let instructions = load(dir.to_str().unwrap()).unwrap();

        assert_eq!(instructions, INVARIANTS);
        assert!(!instructions.contains("legacy"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_soul_is_an_error_instead_of_silently_dropping_identity() {
        let dir = temp_dir("malformed-soul");
        let path = dir.join(SOUL_FILE);
        std::fs::write(&path, [0xff, 0xfe]).unwrap();

        let error = load(dir.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("read"));
        assert!(error.to_string().contains(SOUL_FILE));
        let _ = std::fs::remove_dir_all(dir);
    }
}
