# Releasing

## Prerequisites

- Write access to the repository
- `glab` CLI authenticated, or ability to push tags
- **Generic packages "Reject duplicates" enabled** under
  Settings > Packages and Registries > Generic packages. Without this, a
  re-run of the release pipeline on the same tag silently overwrites the
  bytes uploaded earlier.

## Steps

1. **Update the changelog**: Move items from `## [Unreleased]` to a new version section:

   ```markdown
   ## [Unreleased]

   ## [0.1.0] - 2026-05-13

   ### Added
   - ...
   ```

   Follow [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) conventions. Use categories: Added, Changed, Deprecated, Removed, Fixed, Security.

2. **Bump the crate version** in `Cargo.toml` to match the changelog section.

3. **Commit and merge**: Create a branch, commit the changelog + version bump, merge to master via MR.

4. **Tag the release**: From the merged master:

   ```bash
   git checkout master && git pull
   git tag v0.1.0
   git push origin v0.1.0
   ```

   Tags must match the pattern `v*` (e.g., `v0.1.0`, `v1.0.0-beta.1`).

5. **CI handles the rest**: The release pipeline automatically:
   - Cross-compiles the `.so` for amd64, arm64, and armv7 with `cargo auditable`
     (binaries embed a compressed `Cargo.lock`-equivalent so SBOM tools can
     enumerate Rust deps without source access)
   - Generates per-file `.sha256` sidecars and a combined `SHA256SUMS`
   - Cosign keyless-signs every blob using the GitLab OIDC id token,
     producing `.cosign.bundle` files
   - Uploads everything to the Generic Package Registry under
     `packages/generic/mirror/<tag>/`
   - Extracts the matching changelog section as release notes
   - Creates a GitLab release with all artifacts linked as release assets

6. **Bake into the hub image** (cross-repo coordination):
   - Open an MR on [haproxy-spoa-hub](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub)
   - Add a build stage in the hub's `Dockerfile` that fetches the
     published `libmirror_plugin-<arch>-glibc<version>.so` for each
     supported architecture from this project's Generic Package Registry,
     placing it under `/etc/haproxy-spoa-hub/plugins/libmirror_plugin.so`
   - Pin the version via a `MIRROR_PLUGIN_VERSION` build arg and a
     Renovate-managed default in the hub's CI variables
   - Bump the hub's `Cargo.toml` patch version, add a CHANGELOG entry, cut
     a hub release

## Verifying a release

After CI finishes, verify the release end-to-end from a clean machine:

```sh
make verify-release VERSION=v0.1.0
```

This downloads `SHA256SUMS`, every artifact named in it, and every artifact's
cosign bundle from the package registry, then runs `sha256sum -c` and
`cosign verify-blob` against the project's tag identity.

## Versioning

This project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html):

- **MAJOR**: Breaking changes to the plugin API contract (the set of SPOE
  message arg names and types), or to config parameter names.
- **MINOR**: New config options, new SPOE message arg fields (additive),
  new wire-protocol versions of the request reassembly format.
- **PATCH**: Bug fixes, performance improvements, dependency bumps,
  documentation updates.

Pre-release versions use suffixes: `v0.2.0-alpha.1`, `v0.2.0-beta.1`, `v0.2.0-rc.1`.
