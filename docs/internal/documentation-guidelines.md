# Documentation Guidelines for Epoch

> **Internal Reference**
> Guidelines for maintaining documentation in the Epoch repository, following best practices for LLM-assisted development.

## Documentation Philosophy

Inspired by [claude-skills](https://github.com/Shearerbeard/claude-skills), we maintain a clear separation between internal and external documentation.

### Two Documentation Layers

#### 1. Internal Documentation (This Directory)

**Location**: `docs/internal/`

**Target Audience**: Developers, maintainers, and LLM assistants

**Purpose**:
- Capture architectural decisions and rationale (the "WHY")
- Document coding patterns and conventions (the "HOW")
- Provide context for development sessions
- Record planning decisions and trade-offs

**Examples**:
- `planning/epoch-architecture-philosophy.md` - Design philosophy and architectural patterns
- `planning/coding-style-guide.md` - Coding conventions and best practices
- `documentation-guidelines.md` - This file

**Characteristics**:
- Detailed explanations of design decisions
- Implementation patterns and anti-patterns
- Context that helps developers understand the "why" behind code
- May include incomplete thoughts, TODOs, and planning notes
- Not published in crate documentation

#### 2. External Documentation (User-Facing)

**Location**: `README.md`, doc comments in source code

**Target Audience**: Library users, API consumers

**Purpose**:
- Explain what the library does
- Show how to use the API
- Provide working examples
- Document public interface

**Examples**:
- README.md - Getting started, usage examples
- `/// Doc comments` - API documentation
- Examples directory (if added)

**Characteristics**:
- Focused on usage, not implementation
- Complete, working examples
- Published in crate documentation
- User-centric language

## Documentation Structure

```
/home/user/Epoch/
├── .claude/
│   └── context.md                              # Auto-loaded in Claude Code sessions
├── docs/
│   └── internal/
│       ├── documentation-guidelines.md         # This file
│       └── planning/
│           ├── epoch-architecture-philosophy.md
│           └── coding-style-guide.md
├── src/
│   └── *.rs                                    # Source code with doc comments
└── README.md                                   # Main user-facing documentation
```

## When to Use Each Documentation Type

### Write Internal Documentation When:

- Explaining architectural decisions and trade-offs
- Documenting patterns used throughout the codebase
- Providing context for future development
- Recording planning decisions
- Explaining complex implementation details
- Capturing lessons learned

**Example**: "We use associated types instead of generic parameters for EntityId because..."

### Write External Documentation When:

- Describing public API usage
- Providing getting-started guides
- Showing concrete examples
- Explaining features to users
- Documenting breaking changes in changelogs

**Example**: "To create a new event repository, implement the `EventRepository` trait..."

## Documentation Maintenance

### Claude Code Sessions (.claude/context.md)

The `.claude/context.md` file is **automatically loaded** in Claude Code sessions to provide context. This file should:

- Reference detailed documentation in `docs/internal/`
- Provide quick-start information
- Include common patterns and examples
- Link to relevant internal docs
- Be concise but comprehensive

**Update when**:
- Core patterns change
- New major features are added
- File structure changes
- Common tasks change

### Internal Planning Documents

Files in `docs/internal/planning/` are **source of truth** for architecture and style.

**Update when**:
- Architectural decisions are made
- Patterns are established or changed
- Coding conventions evolve
- New anti-patterns are discovered

### README.md (User-Facing)

The README is the **first thing users see**. It should:

- Show what the library does
- Provide minimal working examples
- Link to additional resources
- Be kept up-to-date with API changes

**Known Issue**: Current README references old `EventContext` API. Needs update to Decider pattern.

**Update when**:
- Public API changes
- New features are added
- Examples become outdated
- Installation instructions change

### Source Code Doc Comments

Doc comments are **published in crate documentation**.

**Guidelines**:
```rust
/// Brief one-line summary.
///
/// Detailed explanation of what this does and how to use it.
///
/// # Type Parameters
///
/// * `E` - The event type
/// * `Err` - The error type
///
/// # Examples
///
/// ```rust
/// use epoch::EventRepository;
///
/// async fn example() {
///     // Working example code
/// }
/// ```
///
/// # Errors
///
/// Returns `Err` if...
///
/// # Panics
///
/// This function panics if... (if applicable)
pub trait EventRepository<E, Err> {
    /// Brief description of this method.
    async fn load(&self) -> Result<Vec<E>, Err>;
}
```

**Update when**:
- Adding public APIs
- Changing function signatures
- Modifying behavior
- Adding new error conditions

## LLM-Assisted Development Guidelines

### For LLM Sessions

When working with LLM assistants (like Claude Code):

1. **Provide Context**: Reference internal documentation files
   - "Review `docs/internal/planning/epoch-architecture-philosophy.md` before making changes"

2. **Maintain Consistency**: Use established patterns
   - "Follow the patterns in `docs/internal/planning/coding-style-guide.md`"

3. **Update Documentation**: When code changes, documentation should too
   - "Update the architecture doc to reflect this new pattern"

4. **Cross-Reference**: Link related documentation
   - Internal docs can reference each other
   - External docs should be self-contained

### For Human Developers

When using Claude Code or other LLM assistants:

1. **Start Sessions with Context**: The `.claude/context.md` file is auto-loaded, but you can explicitly reference deeper docs

2. **Update Documentation Early**: Don't wait until the end to update docs

3. **Separate Internal and External**: Keep implementation details out of user-facing docs

4. **Review Generated Documentation**: LLMs can draft docs, but review for accuracy

## Documentation Review Checklist

### Before Committing Code Changes

- [ ] Public API changes reflected in doc comments
- [ ] README examples still work
- [ ] Internal docs updated if patterns changed
- [ ] `.claude/context.md` updated if major changes
- [ ] No sensitive information in documentation

### Before Release

- [ ] README is up-to-date
- [ ] CHANGELOG.md documents breaking changes
- [ ] Doc comments are comprehensive
- [ ] Examples compile and run
- [ ] Internal docs reflect current architecture

## Common Documentation Tasks

### Adding a New Feature

1. **Code the feature** with doc comments on public APIs
2. **Update internal docs** if new patterns are introduced
3. **Update README** if user-facing functionality changes
4. **Add to context.md** if it's a common task

### Refactoring

1. **Update internal docs** to reflect new architecture
2. **Update doc comments** if public APIs changed
3. **Update README examples** if they're affected
4. **Review context.md** for outdated references

### Fixing Bugs

1. **Update doc comments** if behavior changes
2. **Add to CHANGELOG** if user-facing
3. **Consider adding to coding-style-guide.md** if it reveals an anti-pattern

### Architectural Changes

1. **Document in architecture doc** with rationale
2. **Update coding style guide** if patterns change
3. **Update context.md** for LLM sessions
4. **Update README** if user-facing impacts

## Style Guide

### Writing Style

**Internal Documentation**:
- Technical and detailed
- Explain the "why" behind decisions
- Use examples and code snippets
- Reference external resources
- Can include TODOs and open questions

**External Documentation**:
- Clear and concise
- Focus on "how to use"
- Complete working examples
- Minimal jargon
- Polished and production-ready

### Formatting

**Markdown Conventions**:
```markdown
# Top-Level Heading (One per document)

## Major Section

### Subsection

**Bold** for emphasis
*Italic* for terms
`code` for inline code
```

**Code Blocks**:
````markdown
```rust
// Rust code with syntax highlighting
pub fn example() {
    // ...
}
```
````

**Links**:
```markdown
[Link Text](relative/path/to/file.md)
[External Link](https://example.com)
```

### File Naming

- Use kebab-case: `coding-style-guide.md`
- Be descriptive: `epoch-architecture-philosophy.md` not `arch.md`
- Group by purpose: `planning/`, `decisions/`, etc.

## Documentation TODOs

Current documentation needs:

- [ ] Update README.md to use Decider pattern (not EventContext)
- [ ] Add examples directory with compilable examples
- [ ] Create CHANGELOG.md for tracking releases
- [ ] Add ADR (Architecture Decision Records) for major decisions
- [ ] Document migration path from alpha to 1.0

## Resources

### External References

- [Rust API Guidelines - Documentation](https://rust-lang.github.io/api-guidelines/documentation.html)
- [RFC 1574: API Documentation Conventions](https://rust-lang.github.io/rfcs/1574-more-api-documentation-conventions.html)
- [claude-skills Documentation Philosophy](https://github.com/Shearerbeard/claude-skills)

### Internal References

- [Architecture & Philosophy](planning/epoch-architecture-philosophy.md)
- [Coding Style Guide](planning/coding-style-guide.md)
- [Claude Code Context](../../.claude/context.md)

---

*These guidelines help maintain consistent, valuable documentation across LLM-assisted and traditional development workflows.*
