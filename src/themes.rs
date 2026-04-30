//! Compile-time theme registry. Each theme bundles `overlay.html`, `style.css`,
//! and `app.js` via [`include_str!`]. Selection is via the `?theme=<name>`
//! query string at the HTTP layer; unknown names fall back to `default`.

/// One compiled-in theme.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub html: &'static str,
    pub css: &'static str,
    pub js: &'static str,
}

const THEME_DEFAULT: Theme = Theme {
    name: "default",
    html: include_str!("../web/themes/default/overlay.html"),
    css: include_str!("../web/themes/default/style.css"),
    js: include_str!("../web/themes/default/app.js"),
};

const THEME_MINIMAL: Theme = Theme {
    name: "minimal",
    html: include_str!("../web/themes/minimal/overlay.html"),
    css: include_str!("../web/themes/minimal/style.css"),
    js: include_str!("../web/themes/minimal/app.js"),
};

const THEME_NEON: Theme = Theme {
    name: "neon",
    html: include_str!("../web/themes/neon/overlay.html"),
    css: include_str!("../web/themes/neon/style.css"),
    js: include_str!("../web/themes/neon/app.js"),
};

/// All registered themes.
pub const ALL_THEMES: &[Theme] = &[THEME_DEFAULT, THEME_MINIMAL, THEME_NEON];

/// Resolve `name` (possibly empty / unknown) to a built-in theme. Falls back
/// to `default` when `name` does not match any registered theme.
pub fn resolve(name: Option<&str>) -> Theme {
    let name = name.unwrap_or("").trim();
    if name.is_empty() {
        return THEME_DEFAULT;
    }
    for t in ALL_THEMES {
        if t.name.eq_ignore_ascii_case(name) {
            return *t;
        }
    }
    THEME_DEFAULT
}

/// Look up a theme by exact (case-insensitive) name. Returns `None` if
/// `name` doesn't match any registered theme — used by asset routes that
/// must 404 instead of falling back to `default`.
pub fn find_exact(name: &str) -> Option<Theme> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    ALL_THEMES
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_name_falls_back_to_default() {
        let t = resolve(Some("does-not-exist"));
        assert_eq!(t.name, "default");
    }

    #[test]
    fn empty_name_falls_back_to_default() {
        let t = resolve(Some(""));
        assert_eq!(t.name, "default");
        let t = resolve(None);
        assert_eq!(t.name, "default");
    }

    #[test]
    fn neon_resolves() {
        assert_eq!(resolve(Some("neon")).name, "neon");
        assert_eq!(resolve(Some("NEON")).name, "neon");
    }

    #[test]
    fn minimal_resolves() {
        assert_eq!(resolve(Some("minimal")).name, "minimal");
    }

    #[test]
    fn find_exact_returns_none_for_unknown() {
        assert!(find_exact("does-not-exist").is_none());
    }

    #[test]
    fn find_exact_matches_case_insensitive() {
        assert_eq!(find_exact("NEON").map(|t| t.name), Some("neon"));
    }

    #[test]
    fn find_exact_empty_is_none() {
        assert!(find_exact("").is_none());
        assert!(find_exact("   ").is_none());
    }
}
