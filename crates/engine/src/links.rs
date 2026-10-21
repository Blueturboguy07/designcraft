//! ArtCraft community and project links (Help menu, About, start screen, CLI, MCP).

use serde_json::{Value, json};

/// The app's slug on publikhq.com and GitHub.
pub const APP: &str = "designcraft";
/// The ArtCraft website.
pub const WEBSITE: &str = "https://publikhq.com/adobe";
/// DesignCraft's page on the ArtCraft website.
pub const APP_PAGE: &str = "https://publikhq.com/designcraft";
/// DesignCraft's source repository.
pub const GITHUB: &str = "https://github.com/Blueturboguy07/designcraft";
/// Where to report bugs and request features.
pub const ISSUES: &str = "https://github.com/Blueturboguy07/designcraft/issues";

/// All links as JSON (the `app.links` command).
pub fn all() -> Value {
    json!({
        "website": WEBSITE,
        "appPage": APP_PAGE,
        "github": GITHUB,
        "issues": ISSUES,
    })
}

/// A link by key (`website`, `appPage`, `github`, `issues`).
pub fn get(key: &str) -> Option<&'static str> {
    Some(match key {
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
        assert_eq!(APP_PAGE, format!("https://publikhq.com/{APP}"));
        assert!(GITHUB.ends_with(&format!("/Blueturboguy07/{APP}")));
        assert!(ISSUES.starts_with(GITHUB));
        for k in ["website", "appPage", "github", "issues"] {
            assert!(get(k).is_some_and(|u| u.starts_with("https://")), "{k}");
            assert_eq!(all()[k], get(k).unwrap());
        }
    }
}
