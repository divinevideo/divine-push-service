# Repository Guidelines

This file is the canonical agent guide for this repo. It is tool-agnostic; Claude
Code reads it via an `@AGENTS.md` import in `CLAUDE.md`. Keep Claude-only notes in
`CLAUDE.md`; everything else belongs here.

## Divine Context And Brain

Before broad product, architecture, protocol, cross-repo, service-boundary, or pull-request authoring, review, or modification work, read the shared Divine context primer.

Resolve the context directory and clone it there if it is missing:

```bash
CONTEXT_DIR="${DIVINE_CONTEXT_ROOT:-../divine-context}"
[ -e "$CONTEXT_DIR/.git" ] || gh repo clone divinevideo/divine-context "$CONTEXT_DIR"
```

Use that value as `<context-dir>` below.

The `divine-context` repo is private, so cloning requires GitHub access. If clone, network, or auth fails, continue from the local repo docs and avoid cross-repo assumptions.

Before updating an existing context checkout, verify it is clean and on its default branch. If it is clean and on the default branch, update it with `git -C <context-dir> pull --ff-only`. If it is dirty, on another branch, cannot fast-forward, or network/auth fails, leave it untouched and say the context may be stale.

Read `<context-dir>/AGENT_CONTEXT.md` and follow its instructions. If unavailable, continue from the local repo docs and avoid cross-repo assumptions.

Before working on a pull request, follow `<context-dir>/PR_REVIEW.md` and use `<context-dir>/PR_REVIEW_TEAMS.md` to request the normal team and check takeover authority. Ordinary review remains open to any eligible Divine human. Before modifying a pull-request branch, enforce the mapping and every takeover gate; if the mapping cannot be read, feedback-only review may continue but automated takeover must stop. Request and verify required human review automatically when tooling permits. If the runbook is unavailable, leave the pull request open and report the blocker.

If a Divine Brain search or ask tool is available, you may use it for company memory. Treat it as optional and credentialed: tool names vary by client, and work must continue when Brain is unavailable. When Brain results influence work, cite the returned document ids. Never commit Brain credentials or expose Brain-derived sensitive content in public PRs, issues, branch names, commit messages, code comments, logs, screenshots, release notes, or externally shared agent transcripts.

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
