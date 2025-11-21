# Session Start Checklist

> **Automatic Reference for Claude Code Sessions**
> This file provides a quick checklist and context for starting new development sessions.

**Date**: {SESSION_DATE}

---

## 1. Review Current Work

### Check TODO.md

Current sprint items:
```bash
# Review active work items
grep -A 10 "## Current Sprint" TODO.md
```

**Questions to ask**:
- What's currently in progress?
- What's ready to start?
- Are there any blockers?

### Check CHANGELOG.md

Recent changes:
```bash
# Review recent additions
grep -A 20 "## \[Unreleased\]" CHANGELOG.md
```

**Questions to ask**:
- What changed since last session?
- Are there breaking changes?
- What's ready for next release?

---

## 2. Verify Development Environment

### Quick Checks

```bash
# Ensure we're on the right branch
git branch --show-current

# Check for uncommitted changes
git status

# Verify tests pass
cargo test

# Check code quality
cargo clippy -- -D warnings
cargo fmt --check
```

---

## 3. Core Principles Reminder

### Type Safety
- [ ] Use protected concrete types (smart constructors, private fields)
- [ ] No primitive obsession (wrap String, Uuid, usize)
- [ ] Associated types for domain relationships

### Error Handling
- [ ] Railway-Oriented Programming (Result everywhere)
- [ ] Never panic in business logic (except Evolvers)
- [ ] Use `?` operator for clean composition
- [ ] Custom error types with thiserror

### Functional Purity
- [ ] `decide` and `evolve` are pure functions (no I/O, no side effects)
- [ ] Side effects isolated to repository layer
- [ ] Deterministic state reconstruction

### Documentation Standards
- [ ] NO EMOJIS in code, comments, or commits
- [ ] Doc comments for all public APIs
- [ ] Update CHANGELOG.md for user-facing changes
- [ ] Update TODO.md as work progresses

---

## 4. Session Goals

### Today's Focus

**From TODO.md Current Sprint**:
- [ ] {ITEM_1}
- [ ] {ITEM_2}

**Additional Goals**:
- [ ]
- [ ]

### Success Criteria

By end of session:
- [ ] All tests passing
- [ ] Code formatted and linted
- [ ] TODO.md updated
- [ ] CHANGELOG.md updated (if applicable)
- [ ] Documentation current

---

## 5. Quick References

### File Locations

**Core Documentation**:
- Architecture: `docs/internal/planning/epoch-architecture-philosophy.md`
- Style Guide: `docs/internal/planning/coding-style-guide.md`
- Context: `.claude/context.md`

**Planning**:
- TODO: `TODO.md`
- CHANGELOG: `CHANGELOG.md`
- Workflow: `docs/internal/todo-changelog-workflow.md`

**Code**:
- Traits: `src/decider.rs`
- Strategies: `src/strategies/mod.rs`
- Repositories: `src/repository/`
- Examples: `src/test_helpers/deciders.rs`

### Common Commands

```bash
# Run tests
cargo test

# Run tests for specific backend
cargo test --features esdb
cargo test --features redis

# Check code quality
cargo clippy -- -D warnings
cargo fmt

# Build documentation
cargo doc --open

# Run all checks
cargo fmt && cargo clippy -- -D warnings && cargo test
```

---

## 6. Pre-Commit Checklist

Before committing:
- [ ] Tests pass: `cargo test`
- [ ] No clippy warnings: `cargo clippy -- -D warnings`
- [ ] Code formatted: `cargo fmt`
- [ ] TODO.md updated
- [ ] CHANGELOG.md updated (for user-facing changes)
- [ ] Documentation updated (if API changed)
- [ ] NO EMOJIS in commit message or code

---

## 7. End of Session

Before ending session:
- [ ] Commit work (even if incomplete)
- [ ] Update TODO.md with progress
- [ ] Push to branch
- [ ] Note any blockers or next steps

---

## Session Notes

**What I worked on**:
-

**Progress made**:
-

**Blockers or issues**:
-

**Next session**:
-

---

## Reminders

### Scott Wlaschin's Principles

1. **Make Illegal States Unrepresentable**
   - Use types to prevent invalid states at compile time
   - Smart constructors with validation

2. **Railway-Oriented Programming**
   - Two tracks: success and failure
   - Compose with `?` operator
   - Self-documenting error paths

3. **Type-Driven Development**
   - Let the type system guide implementation
   - Compiler is your friend

### Epoch-Specific

1. **Decider Pattern**: Pure functions for `decide` and `evolve`
2. **Backend Agnostic**: Code works with all three backends
3. **Optimistic Concurrency**: Automatic retry with exponential backoff
4. **Generic Spec Tests**: Test all backends with same suite

---

**This file is automatically referenced at session start. Update it to reflect current project status.**
