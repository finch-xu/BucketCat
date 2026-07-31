//! The built-in update sources, and the single place their manifest URLs are
//! written down.
//!
//! Users pick *among* these; they cannot type a URL. An updater endpoint is a
//! remote-code-execution surface -- whoever serves the manifest decides which
//! signed package the app downloads -- so the set of reachable endpoints stays
//! a compile-time constant. (The minisign signature check means a hostile
//! endpoint still cannot get an unsigned payload installed, but it could pin
//! users to an old vulnerable build, or simply take updates away.)
//!
//! Adding a source -- a mainland-China mirror is the planned next one -- is a
//! one-line append here. Nothing on the frontend changes: `list_update_sources`
//! hands the list over at runtime and the UI renders whatever it gets, looking
//! the display name up by `id` in the i18n dictionary. Deliberately *not*
//! duplicated as a TypeScript constant; two copies of a URL kept in sync by a
//! comment is exactly the thing that drifts.

/// One selectable update source.
pub struct UpdateSource {
    /// Stable identifier persisted in `Settings::update_source` and used as
    /// the i18n lookup key (`settings.updateSourceName.<id>`). Never
    /// translated, never renamed once shipped -- a rename silently invalidates
    /// every existing settings.json.
    pub id: &'static str,
    /// Where the Tauri updater fetches `latest.json` from.
    pub manifest_url: &'static str,
    /// Human-facing download page, offered when the running install cannot
    /// replace itself (a Linux `.deb`/`.rpm`). Per-source rather than one
    /// global constant: a mirror that exists because GitHub is slow to reach
    /// would be pointless if its fallback link still sent users to GitHub.
    pub release_page_url: &'static str,
}

/// Every source the UI may offer, in display order.
///
/// A mirror added here must serve a manifest whose `platforms.*.url` fields
/// point at the mirror too -- the updater follows those URLs verbatim, so a
/// mirrored `latest.json` that still names github.com would hand the manifest
/// over quickly and then stall on the download, which is worse than not
/// mirroring at all.
pub const SOURCES: &[UpdateSource] = &[UpdateSource {
    id: "github",
    manifest_url: "https://github.com/finch-xu/BucketCat/releases/latest/download/latest.json",
    release_page_url: "https://github.com/finch-xu/BucketCat/releases/latest",
}];

/// The source a fresh install starts on, and what `Settings` falls back to
/// when the persisted file predates this feature.
pub const DEFAULT_SOURCE: &str = "github";

/// Resolves a persisted `Settings::update_source` to a manifest URL.
///
/// `None` means "no override": the caller leaves the updater on the endpoint
/// baked into `tauri.conf.json`. That is the deliberate response to an
/// unrecognized id -- a settings.json carrying a source this build has never
/// heard of (hand-edited, or written by a newer version and then downgraded)
/// should still be able to find updates, just from the default endpoint.
pub fn manifest_url_for(id: &str) -> Option<&'static str> {
    SOURCES
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.manifest_url)
}

/// Whether `id` names a source this build knows about. The write path uses
/// this to reject a bad value up front, so an unknown id can only ever reach
/// [`manifest_url_for`] from a file edited outside the app.
pub fn is_known(id: &str) -> bool {
    SOURCES.iter().any(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_known_source() {
        assert_eq!(
            manifest_url_for("github"),
            Some("https://github.com/finch-xu/BucketCat/releases/latest/download/latest.json")
        );
    }

    #[test]
    fn unknown_source_falls_back_to_the_config_default() {
        // Not an error: see the fn doc. A downgrade must not strand a user.
        assert!(manifest_url_for("").is_none());
        assert!(manifest_url_for("china").is_none());
        assert!(manifest_url_for("nonsense").is_none());
    }

    #[test]
    fn the_default_source_is_one_of_the_listed_ones() {
        // Guards the append-a-source edit: renaming an id without updating
        // DEFAULT_SOURCE would leave every fresh install pointing at nothing.
        assert!(is_known(DEFAULT_SOURCE));
        assert!(manifest_url_for(DEFAULT_SOURCE).is_some());
    }

    #[test]
    fn source_ids_are_unique() {
        // Duplicate ids would make `manifest_url_for` silently prefer the
        // first and leave the second unreachable from the settings dropdown.
        let mut ids: Vec<&str> = SOURCES.iter().map(|s| s.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate id in SOURCES");
    }

    /// Every `release_page_url` must be inside the `opener:allow-open-url`
    /// scope, or the "open release page" button silently does nothing when
    /// clicked -- the plugin rejects out-of-scope URLs at runtime, and the UI
    /// only logs that rejection to the console.
    ///
    /// This exists because `SOURCES` is explicitly designed to grow mirrors
    /// (see the doc comment above it), and a mirror's release page will not be
    /// on github.com. Without this test, adding one compiles, ships, and fails
    /// only when a user clicks the button -- with the exact "nothing happens"
    /// symptom the capability entry was introduced to fix.
    #[test]
    fn every_release_page_is_within_the_opener_scope() {
        // `glob` and `serde_json` both already resolve in Cargo.lock (the
        // former via `tauri`, the latter as a direct dependency), so reaching
        // for them here adds no lock entry -- the same bar the rest of this
        // manifest holds itself to.
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json"))
                .expect("capabilities/default.json must be valid JSON");

        let patterns: Vec<glob::Pattern> = capability["permissions"]
            .as_array()
            .expect("`permissions` must be an array")
            .iter()
            .filter(|p| p["identifier"] == "opener:allow-open-url")
            .flat_map(|p| {
                p["allow"]
                    .as_array()
                    .expect("`allow` must be an array")
                    .iter()
                    .map(|entry| {
                        let url = entry["url"].as_str().expect("`url` must be a string");
                        glob::Pattern::new(url).expect("`url` must be a valid glob")
                    })
            })
            .collect();

        assert!(
            !patterns.is_empty(),
            "no opener:allow-open-url entry found -- external links cannot open"
        );

        for s in SOURCES {
            assert!(
                patterns.iter().any(|p| p.matches(s.release_page_url)),
                "source {}'s release page {} is outside the opener scope; \
                 add it to capabilities/default.json",
                s.id,
                s.release_page_url
            );
        }
    }

    #[test]
    fn every_url_is_https() {
        // The updater refuses plain HTTP in release builds anyway; failing
        // here points at the typo instead of at a runtime "endpoint rejected".
        for s in SOURCES {
            assert!(
                s.manifest_url.starts_with("https://"),
                "source {} manifest must use https",
                s.id
            );
            assert!(
                s.release_page_url.starts_with("https://"),
                "source {} release page must use https",
                s.id
            );
        }
    }
}
