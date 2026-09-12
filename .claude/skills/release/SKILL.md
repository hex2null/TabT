---
name: release
description: Release a new version of TabT to GitHub. Handles version bumping, git operations, and GitHub release creation with changelog.
---

# Releasing TabT

This skill streamlines the release process for TabT. It automates:

1. **Version detection** — reads current version from `Cargo.toml` and `Info.plist.in`
2. **Git operations** — commits, creates tag, pushes to origin
3. **GitHub release** — creates release with auto-generated changelog from commits

## Usage

```bash
# Create a release with auto-generated changelog
node .claude/skills/release/driver.mjs create [--version VERSION] [--notes NOTES]

# Show what would be released
node .claude/skills/release/driver.mjs preview

# Create release with custom notes
node .claude/skills/release/driver.mjs create --notes "Custom release notes here"
```

## How it works

1. **Detection**: Reads version from `tabt-app/Cargo.toml`
2. **Staging**: Commits any outstanding changes with a release message
3. **Tagging**: Creates `v{VERSION}` tag and pushes to origin
4. **Release**: Creates GitHub release with:
   - Title: `v{VERSION}`
   - Changelog: Auto-generated from commits since last tag
   - Two-part notes: summary (what changed) + technical details

## Prerequisites

- Git repository with remote origin
- GitHub CLI (`gh`) configured with authentication
- Uncommitted changes staged for release (or will prompt to include)

## What gets included

- **What's new**: Feature summary from commit messages
- **Technical**: Implementation details and affected areas
- **Git reference**: Commit hashes for traceability

## Files involved

- `tabt-app/Cargo.toml` — reads version string
- `bundle/Info.plist.in` — should match version
- `.git/` — commit history for changelog generation
- `CHANGELOG.md` (if it exists) — referenced for historical context
