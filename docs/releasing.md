# Releasing

Releases are built and published by GitHub Actions from a version tag. No
signing keys or repository secrets are required: the workflow uses the built-in
`GITHUB_TOKEN` with `contents: write`.

## Cut a release

1. Bump the version in `apps/rustclip-macos/Cargo.toml` (and keep `Cargo.lock`
   in sync by building once). The tag must match this version exactly.
2. Run the checks locally: `./scripts/check.sh`.
3. Commit and push the bump to `main`.
4. Create and push an annotated tag:

   ```sh
   git tag -a v0.1.0 -m "RustClip 0.1.0"
   git push origin v0.1.0
   ```

5. Watch the **Release** workflow in the Actions tab. The tag push runs three
   jobs:
   - **Resolve version** — rejects a tag that does not look like `vX.Y.Z` or
     does not match `apps/rustclip-macos/Cargo.toml`.
   - **Build universal app** — installs Rust, runs `./scripts/check.sh`, builds
     with `RUSTCLIP_UNIVERSAL=1 ./make-app.sh`, verifies the bundle version and
     that the binary contains both `arm64` and `x86_64`, then packages
     `RustClip-<version>-universal.zip`, `RustClip-<version>-universal.dmg` and
     `SHA256SUMS.txt`.
   - **Publish GitHub release** — creates the release for the tag with the
     generated notes and uploads the three files. If the release already exists,
     it replaces the assets instead.

## Re-running a release

- From the Actions tab, run **Release** with `workflow_dispatch` and the
  existing tag (for example `v0.1.0`). The workflow rebuilds and replaces the
  assets; the release body is kept.
- To regenerate the notes as well, delete the release on GitHub and re-run the
  workflow for the same tag.

## Local universal build

`make-app.sh` builds for the host only by default. For a universal bundle:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
RUSTCLIP_UNIVERSAL=1 ./make-app.sh
lipo -archs dist/RustClip.app/Contents/MacOS/rustclip
```

## Signing and first launch

The bundle is ad-hoc signed (`codesign --sign -`) so macOS accepts it and TCC
keeps a stable identity for the Accessibility permission. It is **not**
notarized. A downloaded copy carries the quarantine flag; the release notes tell
people to right-click the app and choose **Open** the first time. Real
Developer ID signing and notarization would need an Apple Developer account and
secrets, which this workflow deliberately does not assume.
