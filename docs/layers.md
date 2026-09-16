# The three layers

`fcli` exposes the Forgejo API three times over. This is not redundancy — it is the mechanism
that lets the tool claim complete coverage while still having a small, opinionated set of
everyday commands.

```
fcli <noun> <verb>        layer 3   hand-written porcelain   35 groups, 238 commands
fcli raw <group> <op>     layer 2   generated                506 operations, all of them
fcli api <path>           layer 1   generated from nothing   any path under /api/v1
```

Layers 1 and 2 are complete by construction. Layer 3 is deliberately partial and always will be.

## Why coverage is a property of the build

The generator (`cargo xtask codegen`) reads `spec/forgejo-v16.0.4.json` — a de-templated,
key-sorted copy of Forgejo's Swagger 2.0 document, vendored at git tag `v16.0.4` — and emits
four things:

| Emitter | Output | Feeds |
| --- | --- | --- |
| `models` | 246 types | the typed client and every renderer |
| `client` | 506 methods, one per operation | the SDK, and layer 3 |
| `meta` | `static OPS: &[OpMeta]` | layer 2's command tree |
| `fields` | per-model `FieldSpec` tables | `--json` projection and discovery |

`ops_is_a_bijection_with_the_specs_operation_ids`, in
`crates/forgejo-client/src/generated/meta/invariants.rs`, loads the same spec at test time and
asserts a bijection between its `operationId`s and `OPS` — nothing in the spec missing from the
table, nothing in the table absent from the spec. Coverage is therefore a test failure, not a
judgement call, and it fails loudly the moment `cargo xtask update-spec` pulls in new endpoints.

Nobody writes 506 commands. Nobody has to.

## Layer 1 — `fcli api`

The escape hatch, modelled on `gh api`.

```bash
fcli api version
fcli api user --jq .login
fcli api 'repos/{owner}/{repo}/pulls' --paginate --jq '.[].number'
fcli api -X POST -f title=hi -F draft=true 'repos/{owner}/{repo}/issues'
fcli api -i repos/myorg/myrepo
```

- The endpoint is a path relative to `/api/v1`. A leading `/` is optional, and an `/api/v1`
  prefix is accepted and not doubled.
- `{owner}`, `{repo}` and `{branch}` are substituted from the resolved repository.
- `-f/--raw-field` sends strings; `-F/--field` sends JSON types and reads a file when the value
  starts with `@` (`@-` is stdin). They look inverted; they are `gh`'s, exactly.
- `--method` is inferred as `GET`, or `POST` when any field is supplied.
- `--input <file>` supplies the whole body, after which field flags become query parameters.
- `--paginate`, `--slurp`, `-i/--include`, `-H/--header`, `--silent`, `--verbose`.

**Reach for it when** the endpoint is newer than the vendored spec, you want the response
untouched, or you are transcribing a `curl` out of Forgejo's documentation.

## Layer 2 — `fcli raw <group> <op>`

Every operation the specification describes, as a command, with typed flags derived from the
same `Param` values the client's function signatures are derived from.

```bash
fcli raw --help                               # 17 groups
fcli raw repo --help                          # the operations in one group
fcli raw search pull request                  # rank the metadata table by name, summary and path
```

The groups are `activitypub`, `admin`, `artifact`, `git`, `issue`, `misc`, `notify`, `org`,
`package`, `repo`, `run`, `settings`, `task`, `team`, `topic`, `user`, `workflow` — plus
`fcli raw search`.

Each operation's `--help` states the real HTTP method and path, the token scope, and the request
body type:

```console
$ fcli raw repo create-pull-request --help
Create a pull request

HTTP: POST /repos/{owner}/{repo}/pulls
token scope: write:repository
request body: CreatePullRequestOption (application/json, optional)

Usage: fcli raw repo create-pull-request [OPTIONS] [OWNER] [REPO]
```

### Path parameters, positionally or as flags

Both, always. clap cannot make one `Arg` do both, so the generator emits two and merges them
with precedence *flag > positional > repository context > error*. Giving both with different
values is a specific error, not clap's baffling "unexpected argument".

```bash
fcli raw repo list-branches myorg myrepo
fcli raw repo list-branches --owner myorg --repo myrepo
```

When an operation's own path parameter is named `repo`, it takes the long `--repo` and the
global repository flag is available as `-R` only. The `--help` for that command says so on both
arguments.

### Bodies

Request body fields flatten to depth 1 as typed flags. Anything deeper, or an array of objects,
is excluded from the flags and `--help` says so; supply it with `--body-file` and let flags
override individual fields.

```bash
fcli raw repo create-pull-request myorg myrepo --title t --head fix --base main
fcli raw repo create-pull-request myorg myrepo --body-file ./pr.json --title "override"
echo '{"title":"t","head":"fix","base":"main"}' \
  | fcli raw repo create-pull-request myorg myrepo --body-file -
```

### `--dry-run`

Assembles the request and prints it without sending. It needs no token and no network, which
makes the whole generated layer inspectable offline:

```console
$ fcli raw repo create-pull-request myorg myrepo \
    --title "Fix typo" --head fix --base main --dry-run
POST /repos/myorg/myrepo/pulls
content-type: application/json
{
  "base": "main",
  "head": "fix",
  "title": "Fix typo"
}
```

### Path encoding is per-parameter

Most path parameters are percent-encoded with the path-segment set. Parameters on a `PathLike`
allowlist — `filepath`, `treePath`, `ref`, `path`, `filename` — preserve `/`, so this works
rather than 404ing:

```bash
fcli raw repo get-contents myorg myrepo src/main.rs
```

**Reach for it when** the porcelain has no command for what you need. That is most of the API,
by design.

## Layer 3 — the porcelain

35 groups, 238 commands, hand-written and shaped like `gh`.

A command belongs here only if it is *nicer* than layer 2 — which means at least one of:

- **context inference** — resolves the repository, the current branch, or the current user;
- **multi-call orchestration** — `pr create --fill` reads the branch's commits first;
  `repo fork --clone` forks, clones, and renames remotes;
- **interactive fallback** — prompts when a required value is missing and both streams are
  terminals;
- **human rendering** — a table or detail view materially better than pretty-printed JSON.

A porcelain command that is merely a renamed `fcli raw` call is not worth its maintenance. The
binding rules are in [porcelain-conventions.md](porcelain-conventions.md).

**Reach for it** first, for anything it covers.

## A missing porcelain command is never a missing capability

This is the property the layering exists to buy, and it is worth being concrete about, because
the two most obvious gaps in the command list are gaps for completely different reasons.

### `fcli admin badge` — the API has no badge route

`spec/forgejo-v16.0.4.json` contains the string `badge` zero times:

```console
$ grep -c badge spec/forgejo-v16.0.4.json
0
```

There is no endpoint, so there is nothing for any layer to call. `crates/fcli/src/cmd/admin/mod.rs`
says so in prose. If a future Forgejo adds badge routes, `cargo xtask update-spec` makes them
`fcli raw` commands on the day the spec is bumped, and `fcli raw search badge` will find them
before any porcelain does.

### `fcli git-hook` — a gap that cost nothing, and how it closed

Four operations exist in the spec — `repoListGitHooks`, `repoGetGitHook`, `repoEditGitHook`,
`repoDeleteGitHook` — and for several waves no porcelain was written for them. All four were
reachable the entire time:

```bash
fcli raw repo list-git-hooks myorg myrepo
fcli raw repo get-git-hook myorg myrepo pre-receive
fcli raw repo edit-git-hook myorg myrepo pre-receive --content '#!/bin/sh
exit 0'
fcli raw repo delete-git-hook myorg myrepo pre-receive
```

That is the designed failure mode: a missing convenience, with the capability already in your
hands. Contrast it with a hand-written CLI, where "no command for it" and "no way to do it" are
the same sentence.

Layer 3 has since caught up, and the shape of what it added is the argument for the layering:

```bash
fcli git-hook list                            # which hooks are active, in the current repo
fcli git-hook view pre-receive > hook.sh      # the script, verbatim — a table would flatten it
fcli git-hook edit pre-receive -F hook.sh     # or -F - for stdin, or -e for $EDITOR
fcli git-hook disable pre-receive
```

Every one of those is something layer 2 cannot do well: infer the repository, print a multi-line
script without mangling it, read a script from a file or a pipe. And one of them is a *correction*
— there is no `fcli git-hook delete`, because the API's `DELETE` does not remove a hook. Forgejo's
git hooks are a fixed set (`pre-receive`, `update`, `post-receive`) that every repository always
has; the route empties the script and leaves the hook listed as inactive. Layer 2 reports the
route's own name, faithfully; layer 3 is where it gets a name that is true. `fcli git-hook delete`
is accepted, hidden, purely so it can say that.

## One `--jq` expression, three layers

Field names are the API's own snake_case at every layer, with no translation anywhere. That is
what makes this true:

```bash
fcli api 'repos/{owner}/{repo}/pulls' --jq '.[].head.ref'
fcli raw repo list-pull-requests myorg myrepo --jq '.[].head.ref'
fcli pr list --json head --jq '.[].head.ref'
```

Under `gh`'s camelCase, layer 1 would pass `head_repo` through untouched while layers 2 and 3
said `headRepo` — a permanent trap. See [gh-differences.md](gh-differences.md).
