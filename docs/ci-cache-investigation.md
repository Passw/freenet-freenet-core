# CI Cache Rebuild Investigation

**Issue**: #1929 - Dependencies rebuilding despite Rust cache hits in CI

## Root Cause

The CI workflow (`.github/workflows/ci.yml`) uses a shared cache key (`shared-key: "rust-cache"`) across multiple jobs that compile with different configurations:

### Job Configurations

**test_all job:**
- Build: `cargo build --locked` (default features)
- Test: `cargo test --workspace --no-default-features --features trace,websocket,redb`

**clippy_check job:**
- Build: `cargo build --locked --bin fdev --manifest-path ../../crates/fdev/Cargo.toml`
- Clippy: `cargo clippy -- -D warnings`

### Why Rebuilds Occur

1. **Feature Flag Mismatches**: Different feature combinations create different dependency graphs
2. **Target Artifacts Differ**: Test binaries, clippy artifacts, and regular builds have different compilation fingerprints
3. **Cache Hit on Wrong Artifacts**: The cache finds and restores dependencies, but they're compiled with different features than needed

Even though `rust-cache` reports a cache hit (dependency sources are cached), Cargo must recompile because the cached artifacts don't match the current build configuration's requirements.

## Recommended Solutions

### Option 1: Job-Specific Cache Keys (Recommended)

Provide distinct cache keys for each job:

```yaml
# In test_all job
- uses: Swatinem/rust-cache@v2
  with:
    shared-key: "rust-cache-test"
    save-if: ${{ github.ref == 'refs/heads/main' }}

# In clippy_check job
- uses: Swatinem/rust-cache@v2
  with:
    shared-key: "rust-cache-clippy"
    save-if: ${{ github.ref == 'refs/heads/main' }}
```

### Option 2: Align Build Configurations

Use consistent feature flags across all build steps to ensure cache compatibility.

### Option 3: Use Automatic Job Isolation (Simplest)

Remove `shared-key` to let `rust-cache` automatically generate job-specific keys:

```yaml
- uses: Swatinem/rust-cache@v2
  with:
    save-if: ${{ github.ref == 'refs/heads/main' }}
```

## Implementation Note

Workflow files in `.github/workflows/` cannot be modified by Claude due to GitHub App permissions. These changes must be applied manually.

## Expected Outcome

After implementing one of these solutions:
- Cache hits will correspond to the correct compiled artifacts
- Rebuilds will only occur when dependencies actually change
- CI runs will be significantly faster
