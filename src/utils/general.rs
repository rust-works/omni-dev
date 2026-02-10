//! Utility functions and helpers.

use std::fmt;

/// Error type for utility functions.
#[derive(Debug)]
pub enum UtilError {
    /// Invalid input error.
    InvalidInput(String),
    /// I/O error wrapper.
    Io(std::io::Error),
}

impl fmt::Display for UtilError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UtilError::InvalidInput(msg) => write!(f, "Invalid input: {}", msg),
            UtilError::Io(err) => write!(f, "I/O error: {}", err),
        }
    }
}

impl std::error::Error for UtilError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UtilError::InvalidInput(_) => None,
            UtilError::Io(err) => Some(err),
        }
    }
}

impl From<std::io::Error> for UtilError {
    fn from(err: std::io::Error) -> Self {
        UtilError::Io(err)
    }
}

/// Validates input strings.
pub fn validate_input(input: &str) -> Result<(), UtilError> {
    if input.is_empty() {
        return Err(UtilError::InvalidInput("Input cannot be empty".to_string()));
    }

    if input.len() > 1000 {
        return Err(UtilError::InvalidInput("Input too long".to_string()));
    }

    Ok(())
}

/// Formats bytes into human-readable format.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    if unit_index == 0 {
        format!("{} {}", bytes, UNITS[unit_index])
    } else {
        format!("{:.1} {}", size, UNITS[unit_index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_input_valid() {
        assert!(validate_input("valid input").is_ok());
    }

    #[test]
    fn validate_input_empty() {
        assert!(validate_input("").is_err());
    }

    #[test]
    fn validate_input_too_long() {
        let long_input = "a".repeat(1001);
        assert!(validate_input(&long_input).is_err());
    }

    #[test]
    fn format_bytes() {
        assert_eq!(super::format_bytes(0), "0 B");
        assert_eq!(super::format_bytes(512), "512 B");
        assert_eq!(super::format_bytes(1024), "1.0 KB");
        assert_eq!(super::format_bytes(1536), "1.5 KB");
        assert_eq!(super::format_bytes(1048576), "1.0 MB");
        assert_eq!(super::format_bytes(1073741824), "1.0 GB");
    }
}
