# EventCore Release Process

This document describes the release process for the EventCore workspace.

## Automated Release Process

The project uses **[release-plz](https://release-plz.dev/)** for fully automated releases with a two-phase workflow (see ADR-025):

**Phase 1: Release PR Creation** (`.github/workflows/release-plz.yml`, `release-pr` job in the shared workflow)

- Triggered on push to `main` (except release PR merges)
- Analyzes commits using conventional commits for semver detection
- **Enforces lockstep versioning via workspace version inheritance** (see ADR-025 and "Version Lockstep Enforcement" section below)
- Creates/updates release PR with version bumps and changelog
- PR must pass all CI quality gates before merge

**Phase 2: Publication** (`.github/workflows/release-plz.yml`, `release` job in the shared workflow)

- Triggered when release PR is merged (commit message starts with "chore(release):")
- Publishes crates to crates.io in dependency order
- Creates GitHub releases with aggregated changelog content
- Creates git tags for the release

This ensures reliable, automated releases without manual intervention while maintaining:

- Correct dependency ordering during publishing (release-plz)
- Retry logic with exponential backoff for transient failures (release-plz)
- Consistent changelog generation and GitHub releases (release-plz)
- Automated version management and PR creation (release-plz)
- Quality gates enforcement (CI must pass before release PR can merge)

## How It Works

### Package Publishing Order

release-plz automatically determines and uses the correct publishing order based on the dependency graph. It handles the topological sorting internally, ensuring packages are always published in the correct order.

**The dependency graph (automatically handled by release-plz):**

1. `eventcore-types` (no internal dependencies - shared types)
2. `eventcore-macros` (no internal dependencies - proc macros)
3. `eventcore` (depends on eventcore-types and eventcore-macros)
4. `eventcore-testing` (depends on eventcore-types - testing utilities)
5. `eventcore-memory` (depends on eventcore-types)
6. `eventcore-postgres` (depends on eventcore-types)
7. `eventcore-sqlite` (depends on eventcore-types)
8. `eventcore-fs` (depends on eventcore-types)
9. `eventcore-examples` (published example/test crate)

**Note:** All nine crates above (`eventcore-types`, `eventcore-macros`, `eventcore`, `eventcore-testing`, `eventcore-memory`, `eventcore-postgres`, `eventcore-sqlite`, `eventcore-fs`, and `eventcore-examples`) are published to crates.io; the workspace graduated to 1.0.0 in its first stable release. The internal runnable/tooling crates (`eventcore-demo`, `eventcore-bench`, `eventcore-stress`) set `publish = false` and are not published.

release-plz publishes crates in dependency order and retries transient failures with exponential backoff (3 attempts). There is no fixed inter-crate delay in the workflow.

### Version Lockstep Enforcement

**Per ADR-025**, all workspace crates must maintain **identical major.minor.patch versions** (full lockstep). This ensures a clear compatibility guarantee for users: every published crate at a given version is built and released together.

**How it is enforced:**
The workspace `Cargo.toml` defines a single `[workspace.package] version`, and
every member crate declares `version.workspace = true`. There is exactly one
version number in the entire workspace, so version skew is structurally
impossible. When release-plz bumps the version, it bumps the shared workspace
version and all crates move together.

**Example:**

```
Workspace at 0.8.0, eventcore-types gets a breaking change:
  [workspace.package] version: 0.8.0 → 0.9.0
✅ All crates released at 0.9.0
```

**When Major/Minor Bumps Occur:**
When ANY crate requires a major or minor version bump (breaking change or new feature), ALL workspace crates receive the same major.minor bump. This may mean:

- `eventcore` bumps from 0.2.0 → 0.3.0 even if only `eventcore-types` had breaking changes
- Users see an "update" for all crates, even if some have no code changes
- The changelog for unchanged crates will note "Version bump for workspace lockstep compliance"

**Why This Matters:**

- Users can depend on `eventcore = "0.3"` and `eventcore-postgres = "0.3"` knowing they're compatible
- No need for a compatibility matrix or guessing which versions work together
- Follows the same pattern as other workspace projects (e.g., tokio)

### Fallback: Manual Publishing Process

In the unlikely event that the automated process fails, you can publish manually:

```bash
# 1. First, ensure you're on the latest main branch
git checkout main
git pull origin main

# 2. Publish packages in dependency order
cd eventcore-types
cargo publish
cd ..
# Wait for crates.io to index the package (~1-2 minutes)

cd eventcore-macros
cargo publish
cd ..
# Wait for crates.io to index the package

cd eventcore-testing
cargo publish
cd ..
# Wait for crates.io to index the package

cd eventcore-memory
cargo publish
cd ..
# Wait for crates.io to index the package

cd eventcore-postgres
cargo publish
cd ..
# Wait for crates.io to index the package

cd eventcore-sqlite
cargo publish
cd ..
# Wait for crates.io to index the package

cd eventcore-fs
cargo publish
cd ..
# Wait for crates.io to index the package

cd eventcore
cargo publish
cd ..

cd eventcore-examples
cargo publish
cd ..
# Wait for crates.io to index the package

# 3. Create git tags for the release (if not already created)
# Note: release-plz usually creates these tags automatically
git tag -a eventcore-v0.X.Y -m "Release eventcore v0.X.Y"
git tag -a eventcore-types-v0.X.Y -m "Release eventcore-types v0.X.Y"
git tag -a eventcore-macros-v0.X.Y -m "Release eventcore-macros v0.X.Y"
git tag -a eventcore-testing-v0.X.Y -m "Release eventcore-testing v0.X.Y"
git tag -a eventcore-memory-v0.X.Y -m "Release eventcore-memory v0.X.Y"
git tag -a eventcore-postgres-v0.X.Y -m "Release eventcore-postgres v0.X.Y"
git tag -a eventcore-sqlite-v0.X.Y -m "Release eventcore-sqlite v0.X.Y"
git tag -a eventcore-fs-v0.X.Y -m "Release eventcore-fs v0.X.Y"
git tag -a eventcore-examples-v0.X.Y -m "Release eventcore-examples v0.X.Y"

# 4. Push tags to GitHub
git push origin --tags
```

Replace `0.X.Y` with the actual version number.

## Workflow Configuration

The release workflow is split into two parts:

1. **Release PR Creation**: Runs on push to main when it's NOT a release PR merge
2. **Package Publishing**: Runs on push to main when it IS a release PR merge (commit message starts with "chore(release):")

This separation prevents manual version bumps from triggering immediate publishing.

## Credentials and Permissions

### crates.io Publishing Token

The automated workflow uses `CARGO_REGISTRY_TOKEN` from the dedicated 1Password
`Github Secrets` vault to authenticate with crates.io during publishing. GitHub
Actions loads it through the repository-level `OP_SERVICE_ACCOUNT_TOKEN`
bootstrap secret.

**Required permissions:**

- Publish access to all EventCore workspace crates
- Owner or team member with publish rights

**Crate ownership verification:**

```bash
# Check ownership for each crate (requires authentication)
cargo owner --list eventcore
cargo owner --list eventcore-macros
cargo owner --list eventcore-memory
cargo owner --list eventcore-postgres
cargo owner --list eventcore-types
cargo owner --list eventcore-testing
cargo owner --list eventcore-sqlite
cargo owner --list eventcore-fs
```

### Credential Rotation

To rotate the crates.io publishing token:

1. **Generate new token on crates.io:**
   - Log in to [crates.io](https://crates.io/)
   - Navigate to Account Settings → API Tokens
   - Click "New Token"
   - Name: `jwilger GitHub Actions` (or similar)
   - Scopes: `publish-update` (allows publishing new versions)
   - Click "Generate"
   - Copy the token immediately (it won't be shown again)

2. **Update 1Password runtime secret:**
   - Open the `Github Secrets` 1Password vault
   - Find `CARGO_REGISTRY_TOKEN`
   - Replace the `credential` field with the new crates.io token
   - GitHub Actions will load the value through `1password/load-secrets-action`

3. **Revoke old token:**
   - Return to crates.io Account Settings → API Tokens
   - Find the old token
   - Click "Revoke"
   - Confirm revocation

4. **Verify new token works:**
   - Trigger a test release or wait for the next automated release
   - Monitor the GitHub Actions workflow logs
   - Verify successful authentication and publishing

**Recommended rotation schedule:**

- Rotate tokens annually
- Rotate immediately if token is compromised or exposed
- Rotate when team members with token access leave the project

## Troubleshooting

### "Package already published" errors

If you see errors about packages already being published, check:

1. Which packages have actually been published to crates.io
2. Continue manual publishing from the next package in the dependency order

### Version mismatch errors

If you see errors about version requirements not being met:

1. Ensure all workspace dependencies in `Cargo.toml` use the same version
2. Check that the versions match what's in the package's `Cargo.toml`

### Authentication errors during publishing

If you see "no token found" or "authentication failed" errors:

1. Verify `CARGO_REGISTRY_TOKEN` is populated in the `Github Secrets` 1Password vault
2. Check that the token hasn't expired or been revoked on crates.io
3. Verify the token has `publish-update` permissions
4. Rotate the token following the credential rotation procedure above

### Pre-commit hook failures

Always ensure pre-commit hooks pass before attempting to publish. The hooks run:

- Code formatting
- Linting
- Tests
- Type checking

### Version lockstep violations

Lockstep versioning is enforced structurally: the only version number in the
workspace is `[workspace.package] version` in the root `Cargo.toml`, and every
member crate uses `version.workspace = true`. Version skew is only possible if
a member crate replaces `version.workspace = true` with an explicit `version`
field.

**Fix:** restore `version.workspace = true` in the offending crate's
`Cargo.toml` and let the workspace version govern.

**Prevention:**

- Let the automated release workflow manage versions
- Never add an explicit `version` field to a member crate's `Cargo.toml`
