# GitHub Issue Conventions

## Repository

`github.com/jwilger/eventcore`

## Issue Types

| Type        | Purpose                                                    |
| ----------- | ---------------------------------------------------------- |
| **Task**    | A specific piece of implementation or infrastructure work. |
| **Feature** | A request, idea, or new functionality.                     |
| **Bug**     | An unexpected problem or behavior.                         |
| **Meta**    | Repository process or engineering-foundation work.         |

## Labels

**Priority labels:**

- `P0-critical` - Security, data loss, broken builds
- `P1-high` - Major features, important bugs
- `P2-medium` - Default priority
- `P3-low` - Polish, optimization
- `P4-backlog` - Future ideas

**Type labels:**

- `bug` - Something broken
- `enhancement` - New feature or request
- `task` - Work item (refactoring, tests, tooling)
- `epic` - Large feature with sub-issues
- `chore` - Maintenance (audits, cleanup)
- `research` - Investigation / spike
- `documentation` - Docs improvements

## Hierarchy and Relationships

Use GitHub issue task lists or sub-issues where available. If sub-issues are not
enabled, use task-list references in the parent issue body:

### Parent/Child (containment)

Use a checklist in the parent issue body:

```markdown
- [ ] #42 sub-task one
- [ ] #43 sub-task two
```

GitHub renders these as live links and updates the checkbox as referenced issues
close.

### Blocker Relationships

Represent blockers with explicit issue links in the body, for example:

```markdown
Depends on: #15
```

When starting work, check referenced blockers with `gh issue view` before
creating a branch.

## Issue Assignment

When starting work on an issue, assign it to the current authenticated user.

```bash
gh issue edit 42 --repo jwilger/eventcore --add-assignee @me
```

Or via REST:

```bash
curl -fsSL -X PATCH \
  -H "Authorization: token $FORGEJO_TOKEN" \
  -H "Content-Type: application/json" \
  "https://api.github.com/repos/jwilger/eventcore/issues/42" \
  -d '{"assignees": ["jwilger"]}'
```

Unassign when work is complete (the issue will be closed) or if work is
abandoned mid-session.

## Post-Merge Issue Hygiene

After a PR is merged, verify that all issues referenced in the PR body
(`Closes #N`, `Fixes #N`, etc.) were actually closed by Forgejo. Do not rely
solely on auto-close — it can silently fail. Check each referenced issue's
state and close any that remain open.

When closing an issue causes **all** items in a parent issue's checklist to
be checked, ask the user whether the parent issue should also be closed.

## CLI vs REST

- Prefer `tea` (the Forgejo/Gitea CLI) for interactive operations
- Use `curl` against `/api/v1/repos/jwilger/eventcore/...` for scripted
  operations or when `tea` is not available
- There is no Forgejo MCP plugin in use; previously documented
  `mcp__plugin_github_github__*` tools no longer apply
