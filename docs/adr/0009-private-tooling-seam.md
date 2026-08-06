# Development coordination lives outside this repo; the repo publishes decisions, not process

- Status: proposed
- Date: 2026-08
- Deciders: Mike Shearer

## Context and problem statement

The redesign is coordinated with development tooling and a work board
that are not part of this project. Early drafts tracked the board
inside this repo, but a working board is maintainer-operational by
nature: it carries scheduling and review detail that no outside
reader can act on, and it is not written for them. The repo needs a stated seam between what it publishes and
how the work is run.

## Decision drivers

- The repo MUST pass a clone-and-contribute test: a stranger with git
  and cargo alone can build, test, and submit a change guided only by
  tracked files.
- Tracked files MUST NOT reference resources a public reader cannot
  resolve, including work-tracking artifacts.
- The decisions behind the code MUST be public: decision records,
  design annexes, and research stay in this repo.
- Coordination artifacts SHOULD be versioned somewhere, so board
  history survives machines and sessions.

## Considered options

1. Decouple: the board and its tooling configuration move to a
   private companion repo that points back at this checkout; this
   repo publishes decision records (`docs/adr/`), design annexes
   (`docs/design/`), and research (`docs/research/`) only.
2. Track the board here, sanitized, with maintainer-local glosses on
   the tooling it names.
3. Publish the tooling so every process reference is resolvable.

## Decision outcome

Option 1. Option 2 was tried first: a real board accumulates operational
material faster than annotations can cover it, and sanitizing the
cards for publication stripped the operational detail that makes a
board worth keeping. Option 3 was
rejected because the tooling has a life of its own outside this project; publishing it to justify references inverts the
priority. Decoupling gives both sides their best form: the board
keeps full fidelity and its own git history, and this repo's public
tree contains nothing that is not written for an outside reader.

Contributor path: build and test with cargo, open a pull request.
The ADRs state what is being built and why; implementation
sequencing is the maintainer's concern.

## Consequences

- Positive: the public tree is self-contained; every reference in it
  resolves inside it or to a public source.
- Positive: the board is versioned in its own repo instead of living
  as untracked files, and needs no sanitization at all.
- Negative: outside readers cannot see implementation sequencing or
  progress; the ADR statuses (proposed/accepted) are the public
  signal of progress. Accepted.
- Negative: decision records may not cite specific work items; they
  name the work in prose instead, which is looser. Accepted; the
  records carry the substance.
- Negative: the board's pointer at this repo is machine-relative
  configuration in the companion repo; moving checkouts means
  updating it. Accepted; it is one line.

## Links

- Public decision set: [`README.md`](README.md) (this directory)
- Design annexes: [`../design/`](../design/README.md); research:
  [`../research/`](../research/README.md)
