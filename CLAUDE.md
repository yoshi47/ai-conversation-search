# Version Management

## Single Source of Truth

**pyproject.toml** is the single source of truth for the package version.

- `cli.py` reads version dynamically via `importlib.metadata`
- Plugin JSON files (`.claude-plugin/*.json`) must be updated manually when bumping

## Version Locations

When bumping the version, update these files:

1. **pyproject.toml** - `version = "x.y.z"` (PRIMARY - PyPI package version)
2. **.claude-plugin/plugin.json** - `"version": "x.y.z"` (plugin metadata)
3. **.claude-plugin/marketplace.json** - `"version": "x.y.z"` (marketplace metadata)

## Pre-push Hook

A git pre-push hook validates that the version in pyproject.toml doesn't already exist on PyPI.
This prevents CI failures from trying to upload duplicate versions.

## Breaking Changes

Only bump minor version (0.4 → 0.5) for breaking changes. Update SKILL.md minimum version if needed.


# UV

## Testing

You should use uv to run tests:

```bash
uv run pytest tests/ -v

uv run pytest tests/<filename> # run specific test
```