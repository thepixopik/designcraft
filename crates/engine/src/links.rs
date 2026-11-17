//! The AllCraft Studio and project links (Help menu, About, start screen, CLI, MCP).

use serde_json::{Value, json};

/// The app's slug on GitHub.
pub const APP: &str = "designcraft";
/// The AllCraft community discussions.
pub const DISCORD: &str = "https://github.com/thepixopik/designcraft/discussions";
/// The AllCraft Studio website.
pub const WEBSITE: &str = "https://the-allcraft.pages.dev";
/// DesignCraft's live path on The AllCraft Studio.
pub const APP_PAGE: &str = "https://the-allcraft.pages.dev/design/";
/// DesignCraft's source repository.
pub const GITHUB: &str = "https://github.com/thepixopik/designcraft";
/// Where to report bugs and request features.
pub const ISSUES: &str = "https://github.com/thepixopik/designcraft/issues";

/// All links as JSON (the `app.links` command).
pub fn all() -> Value {
    json!({
        "discord": DISCORD,
        "website": WEBSITE,
        "appPage": APP_PAGE,
        "github": GITHUB,
        "issues": ISSUES,
    })
}

/// A link by key (`discord`, `website`, `appPage`, `github`, `issues`).
pub fn get(key: &str) -> Option<&'static str> {
    Some(match key {
        "discord" => DISCORD,
        "website" => WEBSITE,
        "appPage" => APP_PAGE,
        "github" => GITHUB,
        "issues" => ISSUES,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_follow_the_app_slug() {
        assert!(APP_PAGE.starts_with(WEBSITE));
        assert!(GITHUB.ends_with(&format!("/thepixopik/{APP}")));
        assert!(ISSUES.starts_with(GITHUB));
        for k in ["discord", "website", "appPage", "github", "issues"] {
            assert!(get(k).is_some_and(|u| u.starts_with("https://")), "{k}");
            assert_eq!(all()[k], get(k).unwrap());
        }
    }
}
