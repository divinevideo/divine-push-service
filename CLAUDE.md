# diVine Push Service Development Guide

## Build & Test Commands
```bash
# Build the project
cargo build

# Run the application
cargo run

# Run tests
cargo test

# Run a specific test
cargo test test_name

# Run tests with logs visible
RUST_LOG=debug cargo test

# Check code without building
cargo check

# Run clippy
cargo clippy --all-targets --all-features
```

## Code Style Guidelines
- **Imports**: Group by category (std > external > internal), alphabetize within groups
- **Error Handling**: Use the `ServiceError` enum with `thiserror` for typed errors
- **Types**: Use strong typing with descriptive names; leverage `Option<T>` and `Result<T, E>`
- **Naming**: Use snake_case for functions/variables, CamelCase for types/traits
- **Modules**: One module per file, organized by functionality (service boundaries)
- **Logging**: Use `tracing` macros with appropriate log levels
- **Async**: Use `tokio` for async runtime, properly handle task spawning and cancellation
- **Configuration**: Use environment variables for secrets, settings.yaml for defaults
- **Documentation**: Document public functions and modules with /// comments

## Architecture

### Event Kinds
- **3079**: Register push token (encrypted)
- **3080**: Deregister push token (encrypted)
- **3083**: Update notification preferences

### Redis Keys
- `divine:token:{pubkey}` - User's FCM token
- `divine:preferences:{pubkey}` - User's notification preferences (JSON)
- `divine:dedup:{pubkey}:{event_id}` - Deduplication with TTL

### Notification Types
| Type | Kind | Description |
|------|------|-------------|
| Like | 7 | Reactions to user's notes |
| Comment | 1 | Replies to user's notes |
| Follow | 3 | New followers |
| Mention | 1 | Notes mentioning user |
| Repost | 16 | Reposts of user's notes |

## Unblocking Workflow

When you hit a blocker:

1. **Build-check**: `cargo check`
2. **Inspect crate source**: Use scan-crate skill
3. **Run tests**: `cargo test`
4. **Review local docs**: `tree docs/`
5. **Check recent changes**: `git diff`

For more detailed development information, see [Developer Guide](docs/developer-guide.md).
