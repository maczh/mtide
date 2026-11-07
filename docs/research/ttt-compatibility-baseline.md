# ttt compatibility baseline

Research date: 2026-11-07. Scope: the local checkout at
`/home/macro/Work/go/src/github.com/maczh/ttt`, as used by `mtide`.

## Decision

Use the clean public commit
[`afa48edc0b35539423062bafef747dca610fbd5b`](https://github.com/maczh/ttt/commit/afa48edc0b35539423062bafef747dca610fbd5b)
(`origin/main`, dated 2026-11-03) as the first `mtide` compatibility baseline.
It is the public `main` tip of `maczh/ttt`, is addressable by an immutable full
SHA, and is the last public commit before the local oracle-only work.

There is no public tag at that commit. The latest public release tag is
[`v1.9.2-AI`](https://github.com/maczh/ttt/releases/tag/v1.9.2-AI), pointing to
`60d1c5711e075d9ec7702c69c11f51ca6a8fa8df`; it is a valid release fallback but
is older than public `main` and therefore omits the commits between the release
and `afa48edc`.

## Local state that must not be used as the baseline

The checkout is on local `main` at
`3eb1ca669e3554ded66836dbf2efbbf14cfa493b`, four commits ahead of
`origin/main`:

```text
4711d67993d006a166d7c601325a2c7cd5be8775  feat(oracle): tttdump—M1 behavior dump tool
6f3b62b6be0e0f26c0c5f23fdb072a1196ae42f3  feat(oracle): add <Left> / <Right> to oracle scripts
e862a14da68a96c945b0228dfb81b152897649c0  feat(oracle): add editing keys and composable modifiers
3eb1ca669e3554ded66836dbf2efbbf14cfa493b  fix(oracle): ResolveColors in tttdump
```

These commits add the `tttdump`/oracle harness and are useful for later
differential tests, but they are not public `maczh/ttt` history. The checkout
also has dirty/untracked work that must be recorded separately and excluded:

```text
 M internal/oracle/script.go
?? .tmp-goside/
?? cmd/scopecheck/
```

The modified tracked file adds F1–F12 script parsing; the untracked paths are
local syntax/highlighting helper programs. Do not silently fold any of these
into a compatibility claim against `afa48edc`.

## Release and documentation mismatches

- The checkout remote is `https://github.com/maczh/ttt.git`, but `go.mod`, the
  README install commands, and several links still identify
  `github.com/eugenioenko/ttt`.
- `maczh/ttt` has `v1.9.2-AI` as its latest public release; its `Makefile`
  still defaults `VERSION` to `1.9.2-AI` on `main`. This does not make the
  post-release `main` tree a `v1.9.2-AI` source snapshot.
- `mtide`'s [`docs/13-upstream-research.md`](../13-upstream-research.md) cites
  the separate [`eugenioenko/ttt v1.10.0 release`](https://github.com/eugenioenko/ttt/releases/tag/v1.10.0).
  That release is real, but it is not a tag or commit in the `maczh/ttt`
  checkout used for this baseline. Treat it as upstream feature research, not
  as the pinned baseline for this fork, until the repository identity and
  merge point are explicitly decided.

## Reproduction

The following produces a clean, detached checkout of the recommended public
baseline and fails if local changes are present:

```sh
git clone https://github.com/maczh/ttt.git
cd ttt
git fetch --tags origin
git checkout --detach afa48edc0b35539423062bafef747dca610fbd5b
test "$(git rev-parse HEAD)" = "afa48edc0b35539423062bafef747dca610fbd5b"
test -z "$(git status --porcelain)"
git describe --tags --always --dirty
go test ./...
CGO_ENABLED=0 go build -ldflags='-s -w -X main.version=compat-afa48edc' -o bin/ttt ./cmd/ttt
```

For the released fallback, replace the checkout SHA with
`v1.9.2-AI` and verify that it resolves to
`60d1c5711e075d9ec7702c69c11f51ca6a8fa8df`. Every compatibility report should
include the full SHA, `git status --porcelain`, Go version, build flags, and
whether the run used `afa48edc` or the release tag.

## Evidence commands

The local state above was established with:

```sh
git -C /home/macro/Work/go/src/github.com/maczh/ttt status --porcelain=v2 --branch
git -C /home/macro/Work/go/src/github.com/maczh/ttt log --format='%H %ad %s' --date=iso-strict origin/main..main
git -C /home/macro/Work/go/src/github.com/maczh/ttt tag --list --sort=version:refname
gh api repos/maczh/ttt/git/ref/heads/main
gh api repos/maczh/ttt/releases/tags/v1.9.2-AI
gh api repos/eugenioenko/ttt/releases/tags/v1.10.0
```

