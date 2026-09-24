use std::{ffi::OsString, path::PathBuf};

/// Empty variables count as unset.
pub fn default_cache_dir(
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(path) = xdg_cache_home.filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(path).join("cacheexec"));
    }
    if let Some(path) = home.filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(path).join(".cache/cacheexec"));
    }
    None
}

/// The key of a cache directory entry that cacheexec manages, if `name` is one.
pub fn key_of_entry(name: &str) -> Option<&str> {
    let (key, extension) = name.rsplit_once('.')?;
    (matches!(extension, "result" | "active" | "lock")
        && key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    .then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_cache_home_wins_and_empty_values_are_unset() {
        let some = |s: &str| Some(OsString::from(s));
        assert_eq!(
            default_cache_dir(some("/x"), some("/h")),
            Some(PathBuf::from("/x/cacheexec"))
        );
        assert_eq!(
            default_cache_dir(some(""), some("/h")),
            Some(PathBuf::from("/h/.cache/cacheexec"))
        );
        assert_eq!(
            default_cache_dir(some("/x"), None),
            Some(PathBuf::from("/x/cacheexec"))
        );
        assert_eq!(default_cache_dir(None, some("")), None);
        assert_eq!(default_cache_dir(None, None), None);
    }

    #[test]
    fn only_lowercase_sha256_keys_with_known_extensions_are_entries() {
        let key = "0123456789abcdef".repeat(4);
        for extension in ["result", "active", "lock"] {
            assert_eq!(key_of_entry(&format!("{key}.{extension}")), Some(&key[..]));
        }
        assert_eq!(key_of_entry(&format!("{key}.tmp")), None);
        assert_eq!(key_of_entry(&format!("{}.lock", key.to_uppercase())), None);
        assert_eq!(key_of_entry(&format!("{}.lock", &key[1..])), None);
        assert_eq!(key_of_entry(&format!("{}.lock", "g".repeat(64))), None);
        assert_eq!(key_of_entry(&key), None);
    }
}
