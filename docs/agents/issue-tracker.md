# Issue tracker: GitHub

Issues and specs for this repo live as GitHub issues in `maczh/mtide`.
Use the `gh` CLI for all operations.

## Conventions

- Create: `gh issue create --title "..." --body "..."`
- Read: `gh issue view <number> --json number,title,body,labels,comments`
- List: `gh issue list --state open`
- Comment: `gh issue comment <number> --body "..."`
- Apply/remove labels: `gh issue edit <number> --add-label "..."` / `--remove-label "..."`
- Close: comment the resolution, then close the issue.

## Pull requests as a triage surface

PRs as a request surface: no.

## Wayfinding operations

- Map: one issue labelled `wayfinder:map`.
- Child tickets: GitHub sub-issues labelled `wayfinder:research`, `wayfinder:prototype`, `wayfinder:grilling`, or `wayfinder:task`.
- Claim: assign the ticket to the driving developer before work.
- Resolve: comment the answer, close the ticket, then append a context pointer to the map.
- Blocking: use GitHub native issue dependencies.
- Frontier: open, unassigned child issues with no open blockers.
