//! Shared output format type for Atlassian CLI commands.

use clap::ValueEnum;

/// Output/input format for Atlassian content.
#[derive(Clone, Debug, Default, ValueEnum)]
pub enum ContentFormat {
    /// JFM markdown with YAML frontmatter.
    #[default]
    Jfm,
    /// Raw Atlassian Document Format JSON.
    Adf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_jfm() {
        let format = ContentFormat::default();
        assert!(matches!(format, ContentFormat::Jfm));
    }

    #[test]
    fn jfm_variant() {
        let format = ContentFormat::Jfm;
        assert!(matches!(format, ContentFormat::Jfm));
    }

    #[test]
    fn adf_variant() {
        let format = ContentFormat::Adf;
        assert!(matches!(format, ContentFormat::Adf));
    }

    #[test]
    fn debug_format() {
        assert_eq!(format!("{:?}", ContentFormat::Jfm), "Jfm");
        assert_eq!(format!("{:?}", ContentFormat::Adf), "Adf");
    }

    #[test]
    fn clone() {
        let format = ContentFormat::Adf;
        let cloned = format.clone();
        assert!(matches!(cloned, ContentFormat::Adf));
    }
}
