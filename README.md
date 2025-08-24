# omni-dev

[![Crates.io](https://img.shields.io/crates/v/omni-dev.svg)](https://crates.io/crates/omni-dev)
[![Documentation](https://docs.rs/omni-dev/badge.svg)](https://docs.rs/omni-dev)
[![Build Status](https://github.com/rust-works/omni-dev/workflows/CI/badge.svg)](https://github.com/rust-works/omni-dev/actions)
[![License: BSD-3-Clause](https://img.shields.io/badge/License-BSD%203--Clause-blue.svg)](LICENSE)

A comprehensive development toolkit written in Rust.

## Features

- 🚀 Fast and efficient development tools
- 🔧 Extensible architecture
- 📦 Easy to install and use
- 🛡️ Memory safe and reliable

## Installation

### From crates.io

```bash
cargo install omni-dev
```

### From source

```bash
git clone https://github.com/rust-works/omni-dev.git
cd omni-dev
cargo build --release
```

## Usage

### Command Line Interface

```bash
# Run omni-dev
omni-dev --help
```

### As a Library

Add this to your `Cargo.toml`:

```toml
[dependencies]
omni-dev = "0.1.0"
```

Then use it in your Rust code:

```rust
use omni_dev::*;

fn main() {
    // Your code here
    println!("Hello from omni-dev!");
}
```

## Examples

Check out the [examples](examples/) directory for more usage examples.

## Contributing

We welcome contributions! Please see our [Contributing Guidelines](CONTRIBUTING.md) for details.

### Development Setup

1. Clone the repository:
   ```bash
   git clone https://github.com/rust-works/omni-dev.git
   cd omni-dev
   ```

2. Install Rust (if you haven't already):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

3. Build the project:
   ```bash
   cargo build
   ```

4. Run tests:
   ```bash
   cargo test
   ```

5. Run clippy for linting:
   ```bash
   cargo clippy
   ```

6. Format code:
   ```bash
   cargo fmt
   ```

## Documentation

- [API Documentation](https://docs.rs/omni-dev)
- [User Guide](docs/guide.md)
- [Examples](examples/)

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for a list of changes in each version.

## License

This project is licensed under the BSD 3-Clause License - see the [LICENSE](LICENSE) file for details.

## Support

- 📋 [Issues](https://github.com/rust-works/omni-dev/issues)
- 💬 [Discussions](https://github.com/rust-works/omni-dev/discussions)

## Acknowledgments

- Thanks to all contributors who help make this project better!
- Built with ❤️ using Rust