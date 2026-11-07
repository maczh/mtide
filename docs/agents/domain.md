# Domain Docs

## Before exploring

Read:

- `GLOSSARY.md` at the repository root.
- Relevant ADRs under `docs/adr/`.

If these files do not exist, proceed silently. Create them lazily when domain terms or architectural decisions are actually resolved.

## Layout

This is a single-context repository:

```text
/
├── GLOSSARY.md
├── docs/adr/
└── src/
```

## Vocabulary

Use terms defined in `GLOSSARY.md` in issue titles, design proposals, tests, and code comments. If a required concept is not defined, record it for domain modeling rather than inventing a competing synonym.

## ADR conflicts

If a proposal conflicts with an existing ADR, identify the conflict explicitly and explain whether the ADR should be reopened.
