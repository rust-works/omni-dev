//! `clap::ValueEnum` mirror of [`crate::transcript::format::Format`].
//!
//! The library [`Format`] intentionally
//! has no `clap` dependency — that's a hard architectural rule so the
//! `transcript` library remains reusable by non-CLI consumers. This thin
//! enum bridges clap's argument parsing to the library type.

use clap::ValueEnum;

use crate::transcript::format::Format;

/// CLI-visible transcript output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum CliFormat {
    /// SubRip (`.srt`).
    Srt,
    /// WebVTT (`.vtt`).
    Vtt,
    /// Plain text — cue text only, one cue per line.
    Txt,
    /// JSON — the full transcript struct.
    Json,
}

impl From<CliFormat> for Format {
    fn from(value: CliFormat) -> Self {
        match value {
            CliFormat::Srt => Self::Srt,
            CliFormat::Vtt => Self::Vtt,
            CliFormat::Txt => Self::Txt,
            CliFormat::Json => Self::Json,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_variant_maps_to_library_format() {
        assert_eq!(Format::from(CliFormat::Srt), Format::Srt);
        assert_eq!(Format::from(CliFormat::Vtt), Format::Vtt);
        assert_eq!(Format::from(CliFormat::Txt), Format::Txt);
        assert_eq!(Format::from(CliFormat::Json), Format::Json);
    }
}
