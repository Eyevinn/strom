# Contributing to Strom

Thank you for your interest in contributing to Strom! This document provides guidelines and instructions for contributing to the project.

## Development Setup

### Prerequisites

- Rust 1.75 or later
- GStreamer 1.0 development libraries
- trunk (for building the frontend)

### Installing Dependencies

#### Ubuntu/Debian

```bash
sudo apt-get update
sudo apt-get install -y \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav \
    gstreamer1.0-tools

# Install trunk for frontend builds
cargo install trunk

# Add WASM target
rustup target add wasm32-unknown-unknown
```

### Setting Up the Repository

1. Fork and clone the repository
2. Install Git hooks for automatic code quality checks:

```bash
./scripts/install-hooks.sh
```

This will install pre-commit hooks that automatically run:
- `cargo fmt` - Code formatting
- `cargo clippy` - Linting

## Code Quality Standards

All code must pass the following checks before being merged:

### Formatting

Code must be formatted using `rustfmt`:

```bash
cargo fmt --all
```

### Linting

Code must pass clippy with no warnings:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

### Testing

All tests must pass:

```bash
cargo test --workspace
```

## Pre-commit Checks

The pre-commit hook installed by `scripts/install-hooks.sh` will automatically run formatting and linting checks before each commit. If you need to bypass these checks temporarily (not recommended), you can use:

```bash
git commit --no-verify
```

## Building the Project

### Backend Only

```bash
cargo build --package strom
```

### Frontend Only

```bash
cd frontend
trunk build
```

### Complete Build (Backend with Embedded Frontend)

```bash
# Build frontend first
cd frontend
trunk build --release
cd ..

# Build backend with embedded frontend
cargo build --release --package strom
```

### Development Mode

For development, you can run the frontend and backend separately:

```bash
# Terminal 1: Frontend with hot reload
cd frontend
trunk serve

# Terminal 2: Backend
cargo run --package strom
```

## Docker

### Building the Docker Image

```bash
docker build -t strom:latest .
```

The Dockerfile uses cargo-chef for optimal build caching, which significantly speeds up rebuilds.

### Running with Docker

```bash
docker run -p 8080:8080 -v $(pwd)/data:/data strom:latest
```

## Project Structure

```
strom/
├── backend/          # Backend server (Axum + GStreamer)
├── frontend/         # Frontend web UI (egui + WASM)
├── types/            # Shared types between frontend and backend
├── scripts/          # Development scripts
├── .github/          # GitHub Actions CI/CD
└── Dockerfile        # Multi-stage Docker build
```

## Continuous Integration

Our CI pipeline runs on all pull requests and includes:

1. **Format Check** - Verifies code is properly formatted
2. **Clippy** - Runs linting checks
3. **Tests** - Runs all tests
4. **Build** - Builds both frontend and backend
5. **Docker** - Builds Docker image (on master branch only)

All checks must pass before a PR can be merged.

## Making Changes

1. Create a new branch for your changes:
   ```bash
   git checkout -b feature/your-feature-name
   ```

2. Make your changes and ensure all checks pass:
   ```bash
   cargo fmt --all
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace
   ```

3. Commit your changes (pre-commit hooks will run automatically):
   ```bash
   git add .
   git commit -m "Description of your changes"
   ```

4. Push to your fork and create a pull request:
   ```bash
   git push origin feature/your-feature-name
   ```

## Pull Request Guidelines

- Provide a clear description of the changes
- Reference any related issues
- Ensure all CI checks pass
- Keep changes focused and atomic
- Add tests for new functionality
- Update documentation as needed

## Code Review Process

1. A maintainer will review your pull request
2. Address any feedback or requested changes
3. Once approved, a maintainer will merge your PR

## Getting Help

- Open an issue for bug reports or feature requests
- Check existing issues before creating a new one
- Be respectful and constructive in all interactions

## License

By contributing to Strom, you agree that your contributions will be licensed under the same license as the project.
