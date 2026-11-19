# Releasing

A release is a Git tag. Everything else is done by `.github/workflows/release.yml`.

1. **Choose the version** from the Conventional Commits since the last tag: a `feat` is a minor
   release, a `fix` a patch, a `!` after the type or `BREAKING CHANGE` in the body a major one
   (while the version is `0.x`, a breaking change is a minor release).
2. **Set it** in `Cargo.toml` under `[workspace.package]` (the three crates share it) and run
   `cargo update --workspace` so that `Cargo.lock` follows. Open a pull request, let CI pass, merge.
3. **Tag the merge commit** and push the tag:

   ```bash
   git tag -a v0.1.0 -m "v0.1.0"
   git push origin v0.1.0
   ```

The workflow then

- checks that the tag is `vX.Y.Z` and that every crate is at `X.Y.Z`, and runs `fmt`, `clippy`
  and the tests once more;
- builds `chappe`, `chappe-bus` and `chappe-host` for Linux (musl, static) and macOS, on `x86_64`
  and `aarch64`, and packs them as `chappe-<os>-<arch>.tar.gz`;
- publishes the container image `ghcr.io/<owner>/chappe-bus` (tags `X.Y.Z`, `X.Y`, `latest`, for
  `linux/amd64` and `linux/arm64`, made from the binaries above) and the Helm chart
  `oci://ghcr.io/<owner>/charts/chappe-bus` (chart and app version `X.Y.Z`);
- creates the GitHub release with the archives, `SHA256SUMS` and notes written by `git-cliff` from
  the commits (see `cliff.toml`). `install.sh` reads these files.

If a job fails, fix the cause and move the tag only if nothing was published; otherwise release the
next patch version.

## Once, before the first release

- In the settings of the repository, require the check **CI passed** on `main`.
- A package made by a workflow is private at first. After the first release, open each of the two
  packages (`chappe-bus` and `charts/chappe-bus`) under the profile or organization, change the
  visibility to public and link them to the repository.
- Dependabot opens pull requests for Cargo, GitHub Actions and the Dockerfile every week; the
  `Audit` workflow checks the dependencies against RustSec.

## Publishing on crates.io

The crates are not published (`publish = false`). To do it, remove that line, add a description
and `repository` to the three crates and publish `chappe-bus`, `chappe-host` and `chappe-hooks`
(the command `chappe` comes from `chappe-hooks`).
