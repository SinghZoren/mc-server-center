//! Fuzz target: `RelPath::parse` must never panic, and anything it accepts must be canonical
//! and free of traversal. Run with: `cargo +nightly fuzz run relpath` (from crates/msm-fsguard).
#![no_main]

use libfuzzer_sys::fuzz_target;
use msm_fsguard::RelPath;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Ok(rel) = RelPath::parse(text) {
        let s = rel.as_str();
        assert!(!s.is_empty());
        assert!(!s.starts_with('/'));
        assert!(!s.contains('\') && !s.contains(':'));
        assert!(s.split('/').all(|c| !c.is_empty() && c != "." && c != ".."));
        // Canonical: parsing the normalized form yields the same value.
        assert_eq!(RelPath::parse(s).as_ref(), Ok(&rel));
    }
});
