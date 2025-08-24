# Contributing to omni-dev

Thank you for your interest in contributing to omni-dev! We welcome contributions from the community and appreciate your help in making this project better.

## Getting Started

### Development Environment Setup

1. **Install Rust**: Make sure you have Rust installed. We recommend using [rustup](https://rustup.rs/):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. **Clone the repository**:
   ```bash
   git clone https://github.com/rust-works/omni-dev.git
   cd omni-dev
   ```

3. **Install additional tools**:
   ```bash
   # For formatting
   rustup component add rustfmt
   
   # For linting
   rustup component add clippy
   ```

4. **Build and test**:
   ```bash
   cargo build
   cargo test
   ```

## Development Workflow

### Before You Start

- Check the [existing issues](https://github.com/rust-works/omni-dev/issues) to see if your idea is already being worked on
- For major changes, please open an issue first to discuss the proposed changes
- Make sure tests pass locally before submitting a PR

### Making Changes

1. **Create a new branch** for your feature or bug fix:
   ```bash
   git checkout -b feature/your-feature-name
   # or
   git checkout -b fix/your-bug-fix
   ```

2. **Make your changes** following our coding standards (see below)

3. **Add tests** for new functionality

4. **Run the test suite**:
   ```bash
   cargo test
   ```

5. **Run clippy** for linting:
   ```bash
   cargo clippy -- -D warnings
   ```

6. **Format your code**:
   ```bash
   cargo fmt
   ```

7. **Commit your changes** with a clear commit message:
   ```bash
   git commit -m "Add feature: your feature description"
   ```

8. **Push to your fork** and create a pull request

## Coding Standards

### Code Style

- Follow standard Rust formatting (use `cargo fmt`)
- Use meaningful variable and function names
- Add documentation comments (`///`) for public APIs
- Follow Rust naming conventions (snake_case for functions and variables, PascalCase for types)

### Code Quality

- Write comprehensive tests for new functionality
- Ensure all tests pass (`cargo test`)
- Address all clippy warnings (`cargo clippy`)
- Maintain backwards compatibility when possible

### Documentation

- Update the README.md if your changes affect usage
- Add doc comments for new public APIs
- Update CHANGELOG.md for notable changes

## Pull Request Process

1. **Fill out the PR template** with details about your changes
2. **Ensure all CI checks pass** (tests, clippy, formatting)
3. **Request review** from maintainers
4. **Address review feedback** promptly
5. **Squash commits** if requested before merging

## Types of Contributions

### Bug Reports

When filing bug reports, please include:
- A clear description of the problem
- Steps to reproduce the issue
- Expected vs actual behavior
- Your environment (OS, Rust version, etc.)
- Minimal code example if applicable

### Feature Requests

For feature requests, please:
- Describe the use case and motivation
- Explain the proposed solution
- Consider backwards compatibility
- Be open to discussion about implementation

### Documentation

Documentation improvements are always welcome:
- Fix typos or unclear explanations
- Add examples for complex features
- Improve API documentation
- Update outdated information

## Code of Conduct

This project adheres to a [Code of Conduct](CODE_OF_CONDUCT.md). By participating, you are expected to uphold this code.

## Getting Help

If you need help or have questions:
- Check the [documentation](https://docs.rs/omni-dev)
- Search [existing issues](https://github.com/rust-works/omni-dev/issues)
- Start a [discussion](https://github.com/rust-works/omni-dev/discussions)

## Recognition

Contributors will be recognized in our README.md and release notes. Thank you for helping make omni-dev better!

## License

By contributing to omni-dev, you agree that your contributions will be licensed under the BSD 3-Clause License.