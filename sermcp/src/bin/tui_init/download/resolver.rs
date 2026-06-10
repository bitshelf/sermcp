//! Skill context → DownloadProvider resolution.
//!
//! Resolution order:
//!   explicit leaf capability (`download_provider` in config.jsonc)
//!     > compat path alias (the FIRST path segment)
//!     > None (Download test SKIP — never guess a platform).
//!
//! The alias table is the ONLY place path names map to providers — no
//! business logic may test `path == "Rockchip"`.
use super::provider::DownloadProvider;
use super::registry::Registry;

/// Resolve the provider for the selected skill path.
pub fn resolve_provider(
    capability: Option<&str>,
    path: &[String],
) -> Option<&'static dyn DownloadProvider> {
    if let Some(id) = capability.map(str::trim).filter(|id| !id.is_empty()) {
        return Registry::get(id);
    }
    let first = path.first()?;
    let alias = match first.to_ascii_lowercase().as_str() {
        "rockchip" | "rk" => "rockchip",
        "allwinner" | "aw" | "sunxi" => "allwinner",
        _ => return None,
    };
    Registry::get(alias)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn capability_beats_path_alias_and_unknown_is_none() {
        let rockchip = resolve_provider(None, &path(&["Rockchip", "Linux"])).unwrap();
        assert_eq!(rockchip.id(), "rockchip");
        assert_eq!(
            resolve_provider(None, &path(&["allwinner"])).unwrap().id(),
            "allwinner"
        );
        assert_eq!(
            resolve_provider(None, &path(&["SunXi"])).unwrap().id(),
            "allwinner"
        );
        // Explicit capability wins over the path.
        assert_eq!(
            resolve_provider(Some("allwinner"), &path(&["Rockchip"]))
                .unwrap()
                .id(),
            "allwinner"
        );
        // No provider → SKIP, never a guess.
        assert!(resolve_provider(None, &path(&["Rust"])).is_none());
        assert!(resolve_provider(None, &[]).is_none());
    }
}
