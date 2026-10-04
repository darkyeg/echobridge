# Releasing

Two channels, built by the same workflow (`.github/workflows/build.yml`), so a nightly is made exactly like a stable release.

| | Stable | Nightly |
|---|---|---|
| Trigger | push a tag `vX.Y.Z` | every day at 03:00 UTC, or run it by hand |
| Source | the tagged commit | `main`, skipped when it has not changed |
| GitHub release | `vX.Y.Z`, kept forever | `nightly` pre-release, replaced each run |
| File names | `EchoBridge-1.0.1-linux-x86_64.tar.gz` | `EchoBridge-1.0.1-nightly.20261004.abc1234-linux-x86_64.tar.gz` |
| `.deb` version | `1.0.1` | `1.0.1~nightly.20261004.abc1234` (sorts below the release) |
| Installed by default | yes (`releases/latest`) | only with `--nightly` / `-Nightly` |

GitHub's `releases/latest` never points at a pre-release, so `install.sh` and `install.ps1` without options always install stable.

## Make a stable release

1. Set `version` in the workspace `Cargo.toml` and in `Cargo.lock` (`cargo update -w`), and merge it to `main`.
2. Tag and push:
   ```sh
   git tag v1.0.1 && git push origin v1.0.1
   ```
   A tag with a suffix, such as `v1.1.0-rc.1`, is published as a pre-release.
3. The `Release` workflow checks that the tag matches `Cargo.toml`, builds Windows and Linux, and publishes the release with generated notes and these files:
   - Windows: `EchoBridge.exe`, `EchoBridge-Setup.exe`, `SHA256SUMS-windows.txt`
   - Linux: `.tar.gz`, `.deb`, `SHA256SUMS-linux.txt`
   - `install.sh`, `install.ps1`

If the tag was wrong, delete the release and the tag (`gh release delete vX.Y.Z --cleanup-tag`) and tag again.

## Try the nightly

```sh
curl -fsSL https://github.com/darkyeg/echobridge/releases/download/nightly/install.sh | sh -s -- --nightly
```

```powershell
& ([scriptblock]::Create((irm https://github.com/darkyeg/echobridge/releases/download/nightly/install.ps1))) -Nightly
```

Run it again to update. Force a build without waiting for the night: Actions → Nightly → Run workflow → Force.

## Notes

- Linux packages are built on Debian 12, so they run on it and on every newer distribution. CI installs each package on Debian 12 and 13, Ubuntu 24.04, Fedora, Arch and openSUSE Tumbleweed, starting without EchoBridge's libraries, to prove that `install.sh` works there.
- Public repositories get unlimited Actions minutes on the hosted runners. A full build takes roughly 10–15 minutes per system; the cache cuts repeat builds.
- GitHub pauses scheduled workflows after 60 days without repository activity; any push or a manual run restarts them.
- Windows builds are not code-signed yet, so Windows warns on first run.
