# Version Management

## Single Source of Truth

**Cargo.toml** is the single source of truth for the package version.

- Plugin JSON files (`.claude-plugin/*.json`) must be updated when bumping

## Bumping Version

```bash
./scripts/bump-version.sh <new-version>
```

This updates all version locations:
- `Cargo.toml` (Rust package)
- `.claude-plugin/plugin.json` (plugin metadata)
- `.claude-plugin/marketplace.json` (marketplace metadata)
- `bin/ai-conversation-search` (wrapper script version)

## Pre-push Hook

A git pre-push hook validates that the version tag doesn't already exist on the remote.
This prevents CI failures from trying to create duplicate releases.

## Breaking Changes

Only bump minor version (0.6 → 0.7) for breaking changes. Update SKILL.md minimum version if needed.

## Release

Push a version tag to trigger GitHub Actions release:
```bash
git tag v<version>
git push origin v<version>
```

This builds binaries for macOS (arm64/x86_64) and Linux (x86_64) and uploads them to GitHub Releases.

# Testing

```bash
cargo test

cargo test <test_name> # run specific test
```
