# Epoch Documentation

This directory contains internal documentation for the Epoch event sourcing framework.

## Purpose

Internal documentation serves developers, maintainers, and LLM assistants working on the Epoch codebase. Unlike user-facing documentation (README.md, doc comments), these files focus on:

- **WHY** architectural decisions were made
- **HOW** patterns and conventions work
- **WHAT** context is needed for development

## Structure

```
docs/
├── README.md                           # This file
└── internal/
    ├── documentation-guidelines.md     # How we document (meta!)
    └── planning/
        ├── epoch-architecture-philosophy.md  # Core design philosophy
        └── coding-style-guide.md             # Coding conventions
```

## Key Documents

### [Architecture & Philosophy](internal/planning/epoch-architecture-philosophy.md)

Comprehensive guide to Epoch's design philosophy, covering:
- The Decider Pattern and functional purity
- Architectural layers (Traits → Repositories → Strategies → Backends)
- Type safety through associated types
- Backend agnosticism
- Optimistic concurrency model
- Event sourcing principles

**Read this first** when working on the codebase.

### [Coding Style Guide](internal/planning/coding-style-guide.md)

Detailed coding conventions and patterns, including:
- Naming conventions (traits, types, functions)
- Trait patterns and hierarchies
- Error handling strategy
- Testing patterns
- Code organization
- Dependencies and feature flags

**Reference this** when writing or reviewing code.

### [Documentation Guidelines](internal/documentation-guidelines.md)

Meta-documentation about how we maintain docs, covering:
- Internal vs. external documentation
- When to use each type
- Maintenance workflows
- LLM-assisted development guidelines
- Documentation review checklist

**Consult this** when adding or updating documentation.

## For Claude Code Sessions

The `.claude/context.md` file (located at repository root) is automatically loaded in Claude Code sessions. It provides:
- Quick reference to core concepts
- Links to these detailed internal docs
- Common patterns and tasks
- File location reference

## For New Contributors

Start here:
1. Read [Architecture & Philosophy](internal/planning/epoch-architecture-philosophy.md)
2. Review [Coding Style Guide](internal/planning/coding-style-guide.md)
3. Check [Documentation Guidelines](internal/documentation-guidelines.md)
4. Look at example code in `src/test_helpers/deciders.rs`

## For LLM Assistants

These documents are designed to provide context for LLM-assisted development:
- They explain the "why" behind patterns
- They provide concrete examples
- They reference specific file locations
- They maintain consistency across sessions

When assisting with Epoch development, reference these documents to understand:
- Design principles and constraints
- Established patterns and conventions
- Common tasks and their implementations
- Known issues and limitations

## Documentation Updates

Keep these docs synchronized with code changes:

- **Architecture changes** → Update `epoch-architecture-philosophy.md`
- **Pattern changes** → Update `coding-style-guide.md`
- **Documentation process changes** → Update `documentation-guidelines.md`
- **Major features** → Update `.claude/context.md`

## External Documentation

User-facing documentation lives elsewhere:
- **README.md** (repository root) - Getting started, examples
- **Doc comments** (source files) - API documentation
- **Cargo.toml** (metadata) - Crate information

See [Documentation Guidelines](internal/documentation-guidelines.md) for the distinction between internal and external docs.

---

*These internal docs are living documents. Update them as the codebase evolves.*
