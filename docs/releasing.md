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
.\scripts\Set-Version.ps1 -Version 1.0.5
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

Publishing creates `v<version>` from the current `origin/main` commit and uploads the signed MSI, signed appcast, checksums, license, and third-party notices. Installed clients first check `https://updates.resticpal.com/appcast.xml`, then fall back to `https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml`. The updates host must serve the exact GitHub-published `appcast.xml` and `appcast.xml.signature` bytes as a pair; clients reject a mismatched or independently modified mirror.

### Direct-download bridge for 1.0.3 and 1.0.4

Those releases let NetSparkle derive its temporary filename after following redirects. A GitHub release asset redirects to an opaque object path, so a valid MSI can be downloaded under an extensionless GUID and never reach `msiexec`. Version 1.0.5 explicitly assigns a `.msi` staging name, but clients on an older release need a one-time delivery bridge.

The release hook mirrors and verifies the MSI before it advances the signed appcast. A bridge release therefore uses two safe phases:

1. Create the stable GitHub release with only the signed MSI, an MSI-only `SHA256SUMS.txt`, license, notices, and release notes. Do not attach a new appcast yet; clients continue using the previous signed feed.
2. Wait for the release hook to expose `https://updates.resticpal.com/releases/v<version>/resticpal-<version>-x64.msi`. Confirm `HEAD` is a direct HTTP 200 and use a full `GET` to match its length and SHA-256 to the signed CI artifact.
3. Generate the feed only after that verification. `New-UpdateAppcast.ps1` repeats the full remote comparison before reading the private key or signing:

```powershell
.\scripts\New-UpdateAppcast.ps1 `
    -MsiPath C:\path\to\resticpal-1.0.5-x64.msi `
    -Version 1.0.5 `
    -OutputDirectory artifacts\release\v1.0.5\feed `
    -PackageHost UpdatesHost
```

4. Review the enclosure, signature, and final checksums; upload the appcast pair and replace `SHA256SUMS.txt` on the existing release. Explicitly edit the release after all assets exist so the release webhook reruns and atomically advances the mirrored pair.
5. Require byte-identical primary and GitHub fallback feeds, strict signature validation, a previous-client staged path ending in `.msi`, and a signed previous-version-to-new-version Sandbox lifecycle before declaring the release complete.

New clients accept only the exact versioned GitHub or official updates-host paths and still require the pinned Ed25519 package signature. After the bridge population has moved beyond 1.0.4, normal GitHub-hosted enclosures can resume.

## Safety properties

- The private update key is read only by the local release process and never printed or uploaded.
- Appcast generation refuses an unsigned MSI, an unexpected Authenticode publisher, a mismatched product version, or a Dropbox public key that differs from the compiled public key.
- NetSparkle runs in strict Ed25519 mode and verifies the appcast and downloaded MSI.
- NetSparkle is given an explicit unique `.msi` download filename because redirected release assets may otherwise be staged without an extension; Windows Installer selection depends on that suffix.
- Prompted installs use the elevated WinUI application and quiet MSI arguments. When an administrator enables automatic installation, the tray sends the signed enclosure metadata to the LocalSystem service, which independently restricts the release URL, bounds and verifies the MSI, and installs without UAC or installer prompts.
- The service refuses installation while a backup, repository operation, or management operation is active and prevents new backup work while the MSI starts.
- MSI major-upgrade behavior stops and restarts the service. The update hold expires after at most 30 minutes if installation is cancelled before the service is replaced.

Rollback, interrupted-upgrade recovery, and the full Windows 10/11 repair/upgrade matrix still require release qualification before unattended updates are considered broadly production-qualified.
