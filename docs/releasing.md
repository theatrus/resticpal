# Signed Windows releases and updates

resticpal uses two independent signing layers:

- Azure Trusted Signing applies the StackFoundry LLC Authenticode identity to the service, tray, WinUI application, bundled restic executable, and MSI in GitHub Actions.
- A dedicated Ed25519 key signs the NetSparkle appcast and the MSI digest recorded inside that appcast.

The NetSparkle private key is intentionally absent from the repository and GitHub Secrets. Its default local backup is `~/Dropbox/resticpal/keys/updates/NetSparkle_Ed25519.priv`. The matching public-key backup is beside it, while `config/update-public-key.txt` is compiled into the signed WinUI binary. The update key must never be reused for resticpal-server policy or enrollment signatures.

The current public-key SHA-256 fingerprint is:

```text
f581c60bf88a31433837d7d8c329eaf3a31c523c44cc38b30c90e7f1cf5866b4
```

## Set the release version

Use the version helper so the Cargo workspace, Cargo lockfile, WinUI assembly/file metadata, and application manifest move together:

```powershell
.\scripts\Set-Version.ps1 -Version 1.0.1
```

The MSI and generated appcast both derive their version from this synchronized product version. CI runs `Test-VersionConsistency.ps1` and rejects drift.

## Prepare a release

Commit and push the versioned release source, then wait for its successful unsigned `Windows CI` validation on `main`. Ordinary pushes do not consume Trusted Signing quota. Request a signed artifact explicitly from the release commit:

```powershell
gh workflow run ci.yml --ref main
```

Wait for that manually dispatched run to succeed. Manual runs and pushed version tags use Azure OIDC and produce an Authenticode-signed `resticpal-windows-x64` artifact; ordinary `main` pushes, pull requests, and forks remain unsigned. As an alternative, pushing `v<version>` first triggers the signed path for that tag.

Prepare the local release directory without publishing anything:

```powershell
.\scripts\Publish-Release.ps1
```

The script selects only a successful manual or matching `v<version>` tag run for the current commit, downloads its signed MSI, checks the StackFoundry LLC Authenticode identity, confirms the backed-up public key matches the key compiled into resticpal, generates and verifies `appcast.xml` plus `appcast.xml.signature`, and writes checksums below `artifacts/release/v<version>`. Pass `-RunId` to select an explicit signed run.

Review those files. Then publish the GitHub release with reviewed Markdown notes:

```powershell
.\scripts\Publish-Release.ps1 `
    -ReleaseNotesPath C:\path\to\release-notes.md `
    -Publish
```

Publishing creates `v<version>` from the current `origin/main` commit and uploads the signed MSI, signed appcast, checksums, license, and third-party notices. Installed clients check the stable HTTPS URL `https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml`.

## Safety properties

- The private update key is read only by the local release process and never printed or uploaded.
- Appcast generation refuses an unsigned MSI, an unexpected Authenticode publisher, a mismatched product version, or a Dropbox public key that differs from the compiled public key.
- NetSparkle runs in strict Ed25519 mode and verifies the appcast and downloaded MSI.
- Before installation, the elevated WinUI application asks the service for a bounded update hold. The service refuses while a backup or repository operation is active and prevents new backup work while the MSI starts.
- MSI major-upgrade behavior stops and restarts the service. The update hold expires after at most 30 minutes if installation is cancelled before the service is replaced.

Rollback, interrupted-upgrade recovery, and the full Windows 10/11 repair/upgrade matrix still require release qualification before unattended updates are considered.
