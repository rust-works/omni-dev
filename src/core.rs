//! Core functionality for omni-dev

/// Core result type for omni-dev operations
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Main application context
#[derive(Debug, Default)]
pub struct App {
    /// Application configuration
    pub config: Config,
}

/// Configuration structure for omni-dev
#[derive(Debug, Default)]
pub struct Config {
    /// Enable verbose output
    pub verbose: bool,
}

impl App {
    /// Create a new App instance
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new App instance with custom configuration
    pub fn with_config(config: Config) -> Self {
        Self { config }
    }

    /// Run the application
    pub fn run(&self) -> Result<()> {
        if self.config.verbose {
            println!("Running omni-dev in verbose mode");
        }

        println!("omni-dev is ready!");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_creation() {
        let app = App::new();
        assert!(!app.config.verbose);
    }

    #[test]
    fn test_app_with_config() {
        let config = Config { verbose: true };
        let app = App::with_config(config);
        assert!(app.config.verbose);
    }

    #[test]
    fn test_app_run() {
        let app = App::new();
        assert!(app.run().is_ok());
    }
}
