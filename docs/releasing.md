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
.\scripts\Set-Version.ps1 -Version 1.0.6
```

The MSI and generated appcast both derive their version from this synchronized product version. CI runs `Test-VersionConsistency.ps1` and rejects drift.

## Prepare a release

Commit and push the versioned release source, then wait for its successful unsigned `Windows CI` validation on `main`. Ordinary pushes do not consume Trusted Signing quota. Request a signed artifact explicitly from the release commit:

```powershell
gh workflow run ci.yml --ref main
```

Wait for that manually dispatched run to succeed. Manual runs and pushed version tags use Azure OIDC and produce an Authenticode-signed `resticpal-windows-x64` artifact; ordinary `main` pushes, pull requests, and forks remain unsigned.

Every stable release uses two external publication phases. The first phase publishes the signed MSI and a byte-identical copy of the immediately preceding signed appcast pair. This lets the release webhook populate the candidate's direct, non-redirecting package URL without exposing an unqualified new feed or breaking GitHub's `/releases/latest/download/appcast.xml` fallback:

```powershell
.\scripts\Publish-Release.ps1 `
    -ReleaseNotesPath C:\path\to\release-notes.md `
    -Stage
```

`-Stage` requires a clean checkout at the exact `origin/main` commit. It selects only a successful manual or matching version-tag `Windows CI` run for that commit and verifies the StackFoundry LLC Authenticode identity. It first creates a draft release whose hidden body marker records the exact signed CI run, then separately uploads and byte-validates the MSI, an MSI-only `SHA256SUMS.txt`, the license, third-party notices, and the previous release's still-valid signed appcast pair. Only after that complete set is present does it publish the release as stable/latest and trigger the MSI mirror. The MSI asset label also records the signed run ID; every later preparation and finalization reuses that run and verifies the remote MSI digest, even if publishing the release tag starts a newer timestamp-signed build. If creation or upload is interrupted, rerunning the same `-Stage` command discovers the provenance-bearing draft and repairs missing or mismatched known assets while refusing an unexpected target, run, label, or asset. Passing `-RunId` does not weaken those checks: the script still verifies its workflow, commit, conclusion, and signed trigger context.

The current mirror helper copies the candidate MSI before deciding whether an appcast is for the new release. During staging, the intentionally carried-forward appcast still names the previous version, so that hook invocation can report a version-mismatch failure even though it has safely populated the candidate's direct MSI path and left the primary feed unchanged. The direct URL's no-redirect length and SHA-256 verification below is authoritative for proceeding. Finalization replaces the carried pair with the candidate pair and emits a successful deployment event.

Wait for the release hook to expose `https://updates.resticpal.com/releases/v<version>/resticpal-<version>-x64.msi`. A `HEAD` and a full `GET` must return HTTP 200 without a redirect, and the full response must match the signed CI artifact's length and SHA-256.

Now prepare the signed feed locally. `UpdatesHost` is the default and the script repeats the no-redirect, length, and SHA-256 comparison before it reads the private update key:

```powershell
.\scripts\Publish-Release.ps1
```

Preparation downloads the same validated CI artifact, confirms the backed-up public key matches the key compiled into resticpal, generates and verifies `appcast.xml` plus `appcast.xml.signature`, writes final checksums, and records the exact commit, run, and file hashes in the schema-2 `artifacts/release/v<version>/release-manifest.json`. Once that manifest exists, preparation refuses to overwrite the reviewed files implicitly; deliberately remove that versioned artifact directory before starting over. Review and exercise those exact files using the previously published client. In particular, verify the actual NetSparkle staging path ends in `.msi`, then run that MSI through the prompted path and confirm the version, service, and tray binaries all upgrade and their processes restart. Keep the passing Sandbox `result.json`; finalization requires and hash-binds that evidence to the prepared manifest.

Only after those release-blocking checks pass, finalize the already-staged release:

```powershell
.\scripts\Publish-Release.ps1 `
    -ReleaseNotesPath C:\path\to\release-notes.md `
    -UpdateQualificationPath artifacts\windows-sandbox-update\<run-id>\result.json `
    -Finalize
```

`-Finalize` refuses to regenerate reviewed files or proceed without passing schema-1 previous-client qualification evidence. It requires the immediately preceding stable GitHub MSI, exact appcast and candidate hashes, an actual `.msi` staging path, a prompted install, upgraded UI/service/tray file versions, a restarted LocalSystem service, and one cleanly restarted tray process. It then revalidates `release-manifest.json`, the signed MSI, detached appcast signature, checksums, source commit, recorded CI run and tag target, staged GitHub assets (including the exact labeled fallback pair), and a fresh no-redirect `HEAD` plus hashed `GET` from the direct package mirror. Only then does it bind the evidence digest into the manifest, replace the fallback metadata with the exact reviewed and labeled candidate appcast pair and final checksums, and edit the release after both metadata files exist. That final release event lets the server atomically advance the mirrored pair. Re-running `-Stage` before preparation or re-running `-Finalize` is safe: drafts and identical stages are byte-validated and repairable, while finalization requires the same prepared bytes and qualification result. Once `release-manifest.json` exists, `-Stage` refuses to discard it; deliberately restart preparation instead if new staged bytes are required.

After finalization, require byte-identical primary and GitHub fallback appcasts and detached signatures, verify the strict Ed25519 signature again, and confirm the official MSI still returns a direct HTTP 200 with the reviewed length and SHA-256. Installed clients check `https://updates.resticpal.com/appcast.xml` first and `https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml` second. Clients through 1.0.5 stop after a valid-but-current primary feed, so advancing the primary mirror remains part of release completion rather than a best-effort deployment; 1.0.6 and later also consult the secondary feed when the primary is trusted but stale.

### Why every release keeps the direct package URL

Versions 1.0.3 and 1.0.4 let NetSparkle derive their temporary filename after following redirects. A GitHub release asset can redirect to an opaque object path, so a valid MSI may be downloaded under an extensionless GUID and never reach `msiexec`. Version 1.0.5 explicitly assigns a `.msi` staging name, but older installed clients remain eligible for newer updates. Consequently, every current appcast continues to use the direct `updates.resticpal.com` package URL; switching the enclosure back to GitHub would regress those clients and is rejected by the deployment mirror.

`New-UpdateAppcast.ps1` also defaults to `UpdatesHost`. Its explicit `-PackageHost GitHub` option is retained only for isolated compatibility testing; the staged production flow rejects it. New clients accept only the exact versioned GitHub or official updates-host paths and still require the pinned Ed25519 package signature.

## Safety properties

- The private update key is read only by the local release process and never printed or uploaded.
- The staged release carries a byte-identical, signature-verified copy of the previous appcast pair, so installed clients continue using that feed and GitHub's latest fallback remains valid while the direct MSI is mirrored and tested.
- Finalization is mechanically blocked until a passing result from the official immediately preceding client is bound to the exact prepared MSI and appcast hashes.
- Finalization uploads both signed metadata files before emitting the release edit that advances the primary mirror.
- Appcast generation refuses an unsigned MSI, an unexpected Authenticode publisher, a mismatched product version, or a Dropbox public key that differs from the compiled public key.
- NetSparkle runs in strict Ed25519 mode and verifies the appcast and downloaded MSI.
- NetSparkle is given an explicit unique `.msi` download filename because redirected release assets may otherwise be staged without an extension; Windows Installer selection depends on that suffix.
- Prompted installs use the elevated WinUI application and quiet MSI arguments. When an administrator enables automatic installation, the tray sends the signed enclosure metadata to the LocalSystem service, which independently restricts the release URL, bounds and verifies the MSI, and installs without UAC or installer prompts.
- The service refuses installation while a backup, repository operation, or management operation is active and prevents new backup work while the MSI starts.
- MSI major-upgrade behavior stops and restarts the service. The update hold expires after at most 30 minutes if installation is cancelled before the service is replaced.

Rollback, interrupted-upgrade recovery, and the full Windows 10/11 repair/upgrade matrix still require release qualification before unattended updates are considered broadly production-qualified.
