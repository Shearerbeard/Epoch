# TODO and CHANGELOG Workflow

> **Internal Process Documentation**
> This document describes how to use and maintain TODO.md and CHANGELOG.md in the Epoch project.

**Last Updated**: 2025-11-21

---

## Overview

Epoch uses two files to track work:

- **TODO.md**: Planning and work items (what we will do)
- **CHANGELOG.md**: Historical record (what we did)

These files work together to provide visibility into project status and history.

---

## File Purposes

### TODO.md - Project Planning

**Purpose**: Track current work, planned features, and known issues

**Audience**: Developers, contributors, LLM assistants

**Update Frequency**: Continuously (as work progresses)

**Sections**:
- Current Sprint: Active work
- High/Medium/Low Priority: Planned work
- Known Issues: Bugs and problems
- Ideas/Research: Future exploration
- Backlog: Deferred items
- Completed: Recently finished work

### CHANGELOG.md - Project History

**Purpose**: Document all notable changes for users

**Audience**: Library users, maintainers, release managers

**Update Frequency**: With each change, formalized at release

**Sections**:
- Unreleased: Changes not yet released
- Versioned sections: Changes in each release

**Format**: [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)

---

## Daily Workflow

### Starting Work on a New Feature

1. **Check TODO.md**:
   ```bash
   # Review Current Sprint and High Priority sections
   cat TODO.md | head -50
   ```

2. **Move item to "In Progress"**:
   ```markdown
   ### In Progress
   - [ ] Implement LoadDecideAppendWithSnapshot strategy
   ```

3. **Create feature branch**:
   ```bash
   git checkout -b feature/load-decide-append-snapshot
   ```

4. **Do the work** following coding style guide

### Completing Work

1. **Mark TODO item complete**:
   ```markdown
   ### In Progress
   - [x] Implement LoadDecideAppendWithSnapshot strategy
   ```

2. **Add to CHANGELOG.md Unreleased section**:
   ```markdown
   ## [Unreleased]

   ### Added
   - LoadDecideAppendWithSnapshot strategy for combining events and snapshots
   ```

3. **Move to Completed in TODO.md**:
   ```markdown
   ## Completed

   ### Recently Completed
   - [x] Implement LoadDecideAppendWithSnapshot strategy
   ```

4. **Commit with descriptive message**:
   ```bash
   git add src/strategies/mod.rs TODO.md CHANGELOG.md
   git commit -m "Add LoadDecideAppendWithSnapshot strategy

   Implements combined event sourcing and snapshot strategy for improved performance.

   - Loads latest snapshot
   - Applies events since snapshot
   - Validates commands
   - Saves new snapshot

   Closes #42
   "
   ```

### Bug Fixes

1. **Add to Known Issues** in TODO.md:
   ```markdown
   ### Known Issues
   #### Major
   - Version conflict retry logic fails on concurrent updates
   ```

2. **Create fix**

3. **Update CHANGELOG.md**:
   ```markdown
   ### Fixed
   - Version conflict retry logic now correctly handles concurrent updates
   ```

4. **Remove from Known Issues** in TODO.md

---

## Weekly Workflow

### Monday: Sprint Planning

1. **Review TODO.md sections**:
   - Move completed items from "In Progress" to "Completed"
   - Identify new "Current Sprint" items from High Priority
   - Adjust priorities based on project needs

2. **Update "Last Updated" date** in TODO.md

3. **Review CHANGELOG.md**:
   - Ensure all recent work is documented
   - Check for clarity and completeness

### Friday: Sprint Review

1. **Review progress**:
   ```bash
   # Check what was completed this week
   git log --since="1 week ago" --oneline
   ```

2. **Update TODO.md**:
   - Mark completed items
   - Move unfinished items back to appropriate priority

3. **Cleanup CHANGELOG.md**:
   - Group related changes
   - Improve descriptions
   - Add context where needed

---

## Release Workflow

### Preparing a Release

1. **Review CHANGELOG.md Unreleased section**:
   - Ensure all changes are documented
   - Group related changes together
   - Improve clarity and context

2. **Determine version number** using [Semantic Versioning](https://semver.org/):
   - **MAJOR**: Breaking changes (1.0.0 → 2.0.0)
   - **MINOR**: New features, backwards compatible (1.0.0 → 1.1.0)
   - **PATCH**: Bug fixes, backwards compatible (1.0.0 → 1.0.1)
   - **Alpha**: Pre-release (1.0.0-alpha.18 → 1.0.0-alpha.19)

3. **Create release section** in CHANGELOG.md:
   ```markdown
   ## [1.0.0-alpha.19] - 2025-11-21

   ### Added
   - LoadDecideAppendWithSnapshot strategy
   - Comprehensive documentation structure

   ### Changed
   - Improved error messages with entity IDs

   ### Fixed
   - Version conflict retry logic for concurrent updates
   ```

4. **Move Unreleased items** to new release section

5. **Update Cargo.toml** version:
   ```toml
   [package]
   version = "1.0.0-alpha.19"
   ```

6. **Update links** at bottom of CHANGELOG.md:
   ```markdown
   [Unreleased]: https://github.com/Shearerbeard/Epoch/compare/v1.0.0-alpha.19...HEAD
   [1.0.0-alpha.19]: https://github.com/Shearerbeard/Epoch/compare/v1.0.0-alpha.18...v1.0.0-alpha.19
   ```

7. **Commit release changes**:
   ```bash
   git add CHANGELOG.md Cargo.toml
   git commit -m "Release version 1.0.0-alpha.19"
   ```

8. **Tag release**:
   ```bash
   git tag -a v1.0.0-alpha.19 -m "Release version 1.0.0-alpha.19"
   ```

9. **Push with tags**:
   ```bash
   git push origin main
   git push origin v1.0.0-alpha.19
   ```

10. **Create GitHub release** with CHANGELOG excerpt

---

## LLM-Assisted Development

### Starting a Claude Code Session

1. **Review TODO.md**:
   - Claude Code will reference this for current work items
   - Provides context on project status

2. **Check CHANGELOG.md Unreleased**:
   - See what's been added since last session
   - Understand recent changes

3. **Update as you work**:
   - LLM can help update TODO.md and CHANGELOG.md
   - Keep both files synchronized with work

### Best Practices for LLM Sessions

**DO**:
- Ask LLM to update TODO.md when starting/completing work
- Request CHANGELOG entries for significant changes
- Use LLM to improve clarity of entries
- Have LLM check for completeness

**DON'T**:
- Let TODO.md get stale
- Forget to update CHANGELOG.md for user-facing changes
- Use vague descriptions
- Skip context in entries

### Example Prompts

```
"I'm starting work on the LoadDecideAppendWithSnapshot feature.
Please update TODO.md to mark this as in progress."

"I've completed the snapshot strategy. Please:
1. Mark it complete in TODO.md
2. Add an appropriate entry to CHANGELOG.md
3. Suggest related work items"

"Review CHANGELOG.md Unreleased section and improve clarity"
```

---

## Templates

### TODO Item Template

```markdown
- [ ] [Action Verb] [What] [Optional: Why/Context]
  - Location: [File or module]
  - Reason: [Why this is important]
  - Blocks: [What depends on this]
```

**Example**:
```markdown
- [ ] Add validation helpers for Railway-Oriented Programming
  - Location: src/strategies/mod.rs
  - Reason: Simplifies error handling in Deciders
  - Blocks: Decider refactoring work
```

### CHANGELOG Entry Template

```markdown
### [Category]
- [Present tense description of change] ([#issue] if applicable)
  - Additional context if needed
  - Breaking change notice if applicable
```

**Example**:
```markdown
### Added
- Validation helpers for Railway-Oriented Programming composition
  - Enables clean error handling with ? operator
  - Follows Scott Wlaschin's patterns

### Changed
- Retry logic now uses exponential backoff with configurable max retries
  - BREAKING: RetryStrategy trait signature changed
  - Migration: Update implementations to include max_retries parameter
```

---

## CHANGELOG Categories

### Added
New features or capabilities

**Examples**:
- New traits or types
- New backends
- New strategies
- New documentation

### Changed
Changes in existing functionality

**Examples**:
- API improvements
- Performance enhancements
- Refactoring
- Documentation updates

### Deprecated
Soon-to-be removed features

**Examples**:
- Old APIs marked for removal
- Features being replaced

**Note**: Include migration path

### Removed
Removed features

**Examples**:
- Deleted deprecated APIs
- Removed backends

**Note**: Include migration guide

### Fixed
Bug fixes

**Examples**:
- Corrected behavior
- Memory leaks fixed
- Performance issues resolved

### Security
Security-related changes

**Examples**:
- Vulnerability fixes
- Security improvements
- Dependency updates for security

---

## Common Mistakes to Avoid

### TODO.md

❌ **Vague entries**:
```markdown
- [ ] Fix stuff
- [ ] Make it better
```

✅ **Specific entries**:
```markdown
- [ ] Fix version conflict retry logic for concurrent updates
- [ ] Improve error messages to include entity IDs for debugging
```

❌ **Never updating status**:
```markdown
### In Progress (3 months old)
- [ ] Add feature (completed weeks ago)
```

✅ **Keep current**:
```markdown
### Completed
- [x] Add feature (completed 2025-11-15)
```

### CHANGELOG.md

❌ **Past tense**:
```markdown
### Added
- Added new feature
```

✅ **Present tense**:
```markdown
### Added
- Add new feature
```

❌ **No context**:
```markdown
### Changed
- Updated code
```

✅ **Clear context**:
```markdown
### Changed
- Improved retry logic to use exponential backoff for better performance
```

---

## Integration with Git

### Commit Messages

Reference TODO and CHANGELOG updates:

```bash
git commit -m "Add LoadDecideAppendWithSnapshot strategy

Implements combined event sourcing and snapshot strategy.

TODO: Marked as complete
CHANGELOG: Added to Unreleased section

Closes #42
"
```

### Pull Request Template

Include in PR description:

```markdown
## Changes
- [x] Code changes complete
- [x] Tests added
- [x] TODO.md updated
- [x] CHANGELOG.md updated
- [x] Documentation updated

## CHANGELOG Entry
### Added
- LoadDecideAppendWithSnapshot strategy for combining events and snapshots
```

---

## Automation Opportunities

### Git Hooks

**pre-commit**: Check that CHANGELOG.md is updated for code changes

**pre-push**: Ensure no stale "In Progress" items older than 1 week

### CI/CD

- Validate CHANGELOG.md format
- Check for Unreleased section
- Lint TODO.md for broken links

### Scripts

Create helper scripts:
```bash
# scripts/new-todo.sh
# Creates new TODO item with template

# scripts/complete-todo.sh
# Moves item to Completed and prompts for CHANGELOG entry

# scripts/prepare-release.sh
# Automates release preparation steps
```

---

## Summary

**TODO.md and CHANGELOG.md work together**:

1. **Plan work** in TODO.md
2. **Do the work** following style guide
3. **Document changes** in CHANGELOG.md
4. **Complete in TODO.md**
5. **Repeat**

**Key Principles**:
- Keep both files up to date
- Be specific and clear
- Provide context
- Update continuously
- Review regularly

**For LLM Sessions**:
- Reference both files
- Update as you work
- Use for context
- Keep synchronized

---

## References

- [TODO.md](../../TODO.md)
- [CHANGELOG.md](../../CHANGELOG.md)
- [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
- [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
- [Conventional Commits](https://www.conventionalcommits.org/)
