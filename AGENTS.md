# Repository Guidelines

This file is the canonical agent guide for this repo. It is tool-agnostic; Claude
Code reads it via an `@AGENTS.md` import in `CLAUDE.md`. Keep Claude-only notes in
`CLAUDE.md`; everything else belongs here.

## Project Structure & Module Organization
- Service code lives in `src/`, with focused modules for relay listening, preferences, Redis storage, cleanup, and FCM delivery.
- Integration and behavior tests live in `tests/`.
- Runtime configuration lives in `config/`, with deployment assets in `Dockerfile` and `docker-compose.yml`.

## Build, Test, and Development Commands
- `cargo build`: build the service.
- `cargo run`: start the service locally (run the application).
- `cargo check`: run a fast compile-only validation pass (check code without building).
- `cargo clippy --all-targets --all-features`: run lint checks.
- `cargo test`: run the full test suite.
- `cargo test test_name`: run a specific test.
- `RUST_LOG=debug cargo test`: run tests with logs visible.
- Use the docs in `README.md` and `docs/` when changing protocol or operational behavior.

## Coding Style & Naming Conventions
- Use idiomatic Rust with explicit error handling and clear module boundaries.
- Prefer small, focused modules over broad helper collections.
- Keep PRs tightly scoped. Do not mix unrelated cleanup, formatting churn, or speculative refactors into the same change.
- Temporary or transitional code must include `TODO(#issue):` with the tracking issue for removal.

### Detailed Style Guidelines
- **Imports**: Group by category (std > external > internal), alphabetize within groups.
- **Error Handling**: Use the `ServiceError` enum with `thiserror` for typed errors.
- **Types**: Use strong typing with descriptive names; leverage `Option<T>` and `Result<T, E>`.
- **Naming**: Use snake_case for functions/variables, CamelCase for types/traits.
- **Modules**: One module per file, organized by functionality (service boundaries).
- **Logging**: Use `tracing` macros with appropriate log levels.
- **Async**: Use `tokio` for async runtime, properly handle task spawning and cancellation.
- **Configuration**: Use environment variables for secrets, settings.yaml for defaults.
- **Documentation**: Document public functions and modules with `///` comments.

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

## Pull Request Guardrails
- PR titles must use Conventional Commit format: `type(scope): summary` or `type: summary`.
- Set the correct PR title when opening the PR. Do not rely on fixing it afterward.
- If a PR title changes after opening, verify that the semantic PR title check reruns successfully.
- PR descriptions must include a short summary, motivation, linked issue, and manual test plan.
- Changes to protocol behavior, notification handling, or operational configuration should include representative examples or rollout notes when helpful.

## Security & Sensitive Information
- Do not commit secrets, Firebase credentials, private keys, production tokens, or private user data.
- Public issues, PRs, branch names, screenshots, and descriptions must not mention corporate partners, customers, brands, campaign names, or other sensitive external identities unless a maintainer explicitly approves it. Use generic descriptors instead.
