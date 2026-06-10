//! The provider registry — the single place platforms register.
use super::allwinner::AllwinnerDownloadProvider;
use super::provider::DownloadProvider;
use super::rockchip::RockchipDownloadProvider;
use std::sync::LazyLock;

static PROVIDERS: LazyLock<Vec<Box<dyn DownloadProvider>>> = LazyLock::new(|| {
    vec![
        Box::new(RockchipDownloadProvider),
        Box::new(AllwinnerDownloadProvider),
    ]
});

pub struct Registry;

impl Registry {
    pub fn get(id: &str) -> Option<&'static dyn DownloadProvider> {
        PROVIDERS
            .iter()
            .find(|p| p.id() == id)
            .map(|p| p.as_ref() as &'static dyn DownloadProvider)
    }

    #[cfg(test)]
    pub fn ids() -> Vec<&'static str> {
        PROVIDERS.iter().map(|p| p.id()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rockchip_and_allwinner_are_registered() {
        assert_eq!(Registry::ids(), vec!["rockchip", "allwinner"]);
        assert!(Registry::get("rockchip").is_some());
        assert!(Registry::get("allwinner").is_some());
        assert!(Registry::get("starfive").is_none());
    }
}
