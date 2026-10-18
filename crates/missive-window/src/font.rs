//! The system's font. On Linux, GPUI's own "system font" is one fixed family (IBM Plex Sans) that
//! few systems have; without it the text falls back on a font whose other weights are never used,
//! so bold never shows. The window takes fontconfig's preference for sans-serif instead, the
//! first family of it that is installed, as the rest of the desktop does.

use std::path::{Path, PathBuf};

use fontconfig_parser::FontConfig;

/// The family the window writes in; `None` leaves it to GPUI.
pub fn sans_serif(installed: &[impl AsRef<str>]) -> Option<String> {
    let config = std::env::var_os("FONTCONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/fonts/fonts.conf"));
    preferred(&config, installed)
}

/// The first installed family of the sans-serif aliases of a configuration and the files it
/// includes (the user's own among them), in their order.
fn preferred(config: &Path, installed: &[impl AsRef<str>]) -> Option<String> {
    let mut parsed = FontConfig::default();
    parsed.merge_config(config).ok()?;
    parsed
        .aliases
        .iter()
        .filter(|alias| {
            alias.alias.eq_ignore_ascii_case("sans-serif")
                || alias.alias.eq_ignore_ascii_case("sans serif")
                || alias.alias.eq_ignore_ascii_case("sans")
        })
        .flat_map(|alias| {
            alias
                .prefer
                .iter()
                .chain(&alias.accept)
                .chain(&alias.default)
        })
        .find(|family| {
            installed
                .iter()
                .any(|name| name.as_ref() == family.as_str())
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(folder: &Path, user: &str) -> PathBuf {
        std::fs::write(
            folder.join("user.conf"),
            format!(
                "<?xml version=\"1.0\"?><fontconfig><alias><family>sans-serif</family>\
                 <prefer><family>{user}</family></prefer></alias></fontconfig>"
            ),
        )
        .unwrap();
        let main = folder.join("fonts.conf");
        std::fs::write(
            &main,
            format!(
                "<?xml version=\"1.0\"?><fontconfig><include>{}</include>\
                 <alias><family>monospace</family><prefer><family>Mono One</family></prefer></alias>\
                 <alias><family>sans-serif</family><prefer><family>Missing Sans</family>\
                 <family>Second Sans</family></prefer><default><family>Last Sans</family></default>\
                 </alias></fontconfig>",
                folder.join("user.conf").display()
            ),
        )
        .unwrap();
        main
    }

    #[test]
    fn the_first_installed_sans_serif_family_is_taken_the_users_first() {
        let folder = tempfile::tempdir().unwrap();
        let main = config(folder.path(), "Chosen Sans");
        let installed = ["Mono One", "Second Sans", "Chosen Sans", "Last Sans"];
        assert_eq!(preferred(&main, &installed).as_deref(), Some("Chosen Sans"));
        let installed = ["Mono One", "Second Sans", "Last Sans"];
        assert_eq!(preferred(&main, &installed).as_deref(), Some("Second Sans"));
        assert_eq!(
            preferred(&main, &["Last Sans"]).as_deref(),
            Some("Last Sans")
        );
    }

    #[test]
    fn without_a_configuration_or_an_installed_family_gpui_chooses() {
        let folder = tempfile::tempdir().unwrap();
        let main = config(folder.path(), "Chosen Sans");
        assert_eq!(preferred(&main, &["Mono One"]), None);
        assert_eq!(
            preferred(&folder.path().join("none.conf"), &["Last Sans"]),
            None
        );
    }
}
