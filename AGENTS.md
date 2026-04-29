# Repository Guidelines

## Project Structure & Module Organization
- Service code lives in `src/`, with focused modules for relay listening, preferences, Redis storage, cleanup, and FCM delivery.
- Integration and behavior tests live in `tests/`.
- Runtime configuration lives in `config/`, with deployment assets in `Dockerfile` and `docker-compose.yml`.
- Repo-specific maintainer guidance already exists in `CLAUDE.md`; keep that aligned with this file rather than duplicating detailed implementation advice.

## Build, Test, and Development Commands
- `cargo build`: build the service.
- `cargo run`: start the service locally.
- `cargo check`: run a fast compile-only validation pass.
- `cargo clippy --all-targets --all-features`: run lint checks.
- `cargo test`: run the full test suite.
- Use the docs in `README.md` and `docs/` when changing protocol or operational behavior.

## Coding Style & Naming Conventions
- Use idiomatic Rust with explicit error handling and clear module boundaries.
- Prefer small, focused modules over broad helper collections.
- Keep PRs tightly scoped. Do not mix unrelated cleanup, formatting churn, or speculative refactors into the same change.
- Temporary or transitional code must include `TODO(#issue):` with the tracking issue for removal.

## Pull Request Guardrails
- PR titles must use Conventional Commit format: `type(scope): summary` or `type: summary`.
- Set the correct PR title when opening the PR. Do not rely on fixing it afterward.
- If a PR title changes after opening, verify that the semantic PR title check reruns successfully.
- PR descriptions must include a short summary, motivation, linked issue, and manual test plan.
- Changes to protocol behavior, notification handling, or operational configuration should include representative examples or rollout notes when helpful.

## Security & Sensitive Information
- Do not commit secrets, Firebase credentials, private keys, production tokens, or private user data.
- Public issues, PRs, branch names, screenshots, and descriptions must not mention corporate partners, customers, brands, campaign names, or other sensitive external identities unless a maintainer explicitly approves it. Use generic descriptors instead.
