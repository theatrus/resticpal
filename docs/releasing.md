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
.\scripts\Set-Version.ps1 -Version 1.0.7
```

The MSI and generated appcast both derive their version from this synchronized product version. CI runs `Test-VersionConsistency.ps1` and rejects drift.

## Prepare a release

Commit and push the versioned release source, then wait for its successful unsigned `Windows CI` validation on `main`. Ordinary pushes do not consume Trusted Signing quota. Request a signed artifact explicitly from the release commit:

```powershell
gh workflow run ci.yml --ref main
```

Wait for that manually dispatched run to succeed. Manual runs and pushed version tags use Azure OIDC and produce an Authenticode-signed `resticpal-windows-x64` artifact; ordinary `main` pushes, pull requests, and forks remain unsigned.

Every stable release uses two external publication phases and two deliberately isolated feed generations. Clients through v1.0.6 remain on the legacy `appcast.xml`; every v1.0.7-and-later client reads only `appcast-v2.xml`. The legacy pair is frozen forever at the exact signed v1.0.5 bytes (SHA-256 `24bf69c40dc2fc81d4c1db1f23d4d44bd43c53ec26b1f9f0457eb48c9b393c87` and `a8c669aec1223ae927920d85509168940d6528d62fe20865a9de765ac9a4e6f2`). This prevents an automatically enabled v1.0.5 or v1.0.6 service from seeing a v1.0.7 package whose safe staging implementation it does not contain.

The first phase publishes the signed MSI with that byte-pinned legacy pair. Releases after v1.0.7 also carry the immediately preceding signed v2 pair; v1.0.7 carries no v2 pair while staged. This lets the release webhook populate the candidate's direct, non-redirecting package URL without exposing an unqualified new feed:

```powershell
.\scripts\Publish-Release.ps1 `
    -ReleaseNotesPath C:\path\to\release-notes.md `
    -Stage
```

`-Stage` requires a clean checkout at the exact `origin/main` commit. It selects only a successful manual or matching version-tag `Windows CI` run for that commit and verifies the StackFoundry LLC Authenticode identity. It first creates a draft release whose hidden body marker records the exact signed CI run, then separately uploads and byte-validates the MSI, checksums, license, notices, and frozen legacy pair, plus the previous v2 pair when one exists. The pinned legacy assets are labeled `Frozen legacy feed v1.0.5`; finalization never replaces them. Only after the complete stage is present does the script publish it as stable/latest and trigger the MSI mirror. The MSI asset label also records the signed run ID; every later preparation and finalization reuses that run and verifies the remote MSI digest. Interrupted staging is repairable only from known, byte-identical assets and labels.

The mirror copies and validates the candidate MSI without advancing either feed. During staging, v1.0.7 has no v2 feed and later releases retain the preceding v2 pair. The direct URL's no-redirect length and SHA-256 verification below is authoritative for proceeding. Finalization adds or replaces only the v2 pair (plus its checksum record) and emits the deployment event; the legacy pair must stay byte-identical.

Wait for the release hook to expose `https://updates.resticpal.com/releases/v<version>/resticpal-<version>-x64.msi`. A `HEAD` and a full `GET` must return HTTP 200 without a redirect, and the full response must match the signed CI artifact's length and SHA-256.

Now prepare the signed feed locally. `UpdatesHost` is the default and the script repeats the no-redirect, length, and SHA-256 comparison before it reads the private update key:

```powershell
.\scripts\Publish-Release.ps1
```

Preparation downloads the same validated CI artifact, confirms the backed-up public key matches the key compiled into resticpal, and generates and verifies `feed/appcast-v2.xml` plus its detached signature. It writes final checksums and a schema-4 `artifacts/release/v<version>/release-manifest.json` that binds the candidate enclosure, frozen legacy bytes, and qualification strategy. For the one-time v1.0.6→v1.0.7 rescue transition it also generates `probe/appcast-v2-probe.xml`, its production-key signature, and a tiny v1.0.8 sentinel payload whose enclosure signature is deliberately invalid. Probe files are qualification inputs and are never release assets.

Once the manifest exists, preparation refuses to overwrite reviewed files implicitly. Review and exercise those exact files using the immediately previous published client. The prompted path must preserve the actual `.msi` staging extension. The automatic path must download to ProgramData and start a LocalSystem session-0 `/qn /norestart` MSI without confirmation, installer dialog, or UAC. Keep both passing `result.json` files; finalization hash-binds both to the manifest.

Only after those release-blocking checks pass, finalize the already-staged release:

```powershell
.\scripts\Publish-Release.ps1 `
    -ReleaseNotesPath C:\path\to\release-notes.md `
    -UpdateQualificationPath artifacts\windows-sandbox-update\<prompted-run-id>\result.json `
    -AutomaticUpdateQualificationPath artifacts\windows-sandbox-update\<automatic-run-id>\result.json `
    -Finalize
```

`-Finalize` refuses to regenerate reviewed files or proceed without both qualification results. Prompted evidence remains schema 1. Ordinary automatic evidence is schema 1 and must prove dispatch by the published client tray. Only the exact v1.0.6→v1.0.7 transition accepts schema-2 automatic evidence with dispatcher `qualification-harness-via-published-service-ipc`. That bridge must prove an Accepted protocol-v3 `install_update` carrying the exact prepared enclosure, then prove the upgraded protocol-v4 tray independently fetches the exact signed invalid-package probe, emits `update.started` followed by `update.failed/update_signature_invalid`, leaves no matching final, partial, or alternate probe staging entry, and launches zero probe `msiexec` processes. The special bridge exists because the published v1.0.6 tray lacks the named-pipe busy retry; it is not permitted on any later transition.

Finalization also requires the immediately preceding stable GitHub MSI, candidate hashes, actual `.msi` staging, upgraded file versions, restarted LocalSystem service, and one replacement tray. It revalidates the schema-4 manifest, signed MSI, v2 signature, checksum set, source/run/tag identity, the exact frozen legacy assets, any staged previous v2 pair, and a fresh no-redirect mirror download. Only then does it bind both evidence snapshots, add or replace the exact reviewed `appcast-v2.xml` pair and checksums, and edit the release. It never clobbers `appcast.xml` or its signature.

The qualification evidence loader reads each result once while denying writers, then parses and hashes that same byte snapshot. `Test-VersionConsistency.ps1` runs the table-driven `Test-ReleaseQualification.ps1` suite in CI, including malformed schemas and types, missing or partial evidence, swapped modes, duplicate result files, and partial or mismatched manifest bindings.

After finalization, require byte-identical v2 primary and GitHub fallback appcasts and detached signatures, verify the strict Ed25519 signature again, and confirm the official MSI still returns a direct HTTP 200 with the reviewed length and SHA-256. v1.0.7-and-later clients check `https://updates.resticpal.com/appcast-v2.xml` first and `https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml` second. The legacy `appcast.xml` endpoints remain frozen at v1.0.5. Consequently v1.0.7 is distributed as an explicit manual/rescue MSI to v1.0.6 installations, not as an automatic legacy-feed update.

### Why every release keeps the direct package URL

Versions 1.0.3 and 1.0.4 let NetSparkle derive their temporary filename after following redirects. A GitHub release asset can redirect to an opaque object path, so a valid MSI may be downloaded under an extensionless GUID and never reach `msiexec`. Version 1.0.5 explicitly assigns a `.msi` staging name, but older installed clients remain eligible for newer updates. Consequently, every current appcast continues to use the direct `updates.resticpal.com` package URL; switching the enclosure back to GitHub would regress those clients and is rejected by the deployment mirror.

`New-UpdateAppcast.ps1` also defaults to `UpdatesHost`. Its explicit `-PackageHost GitHub` option is retained only for isolated compatibility testing; the staged production flow rejects it. New clients accept only the exact versioned GitHub or official updates-host paths and still require the pinned Ed25519 package signature.

## Safety properties

- The private update key is read only by the local release process and never printed or uploaded.
- Every release carries the byte-pinned v1.0.5 legacy pair, and finalization is forbidden from advancing it.
- A v1.0.7 stage exposes no v2 feed; later stages carry only the previous signed v2 pair until qualification completes.
- Finalization is mechanically blocked until a passing result from the official immediately preceding client is bound to the exact prepared MSI and appcast hashes.
- Finalization uploads both signed v2 metadata files before emitting the release edit that advances the v2 mirror.
- Appcast generation refuses an unsigned MSI, an unexpected Authenticode publisher, a mismatched product version, or a Dropbox public key that differs from the compiled public key.
- NetSparkle runs in strict Ed25519 mode and verifies the appcast and downloaded MSI.
- NetSparkle is given an explicit unique `.msi` download filename because redirected release assets may otherwise be staged without an extension; Windows Installer selection depends on that suffix.
- Prompted installs use the elevated WinUI application and quiet MSI arguments. When an administrator enables automatic installation, the tray sends the signed enclosure metadata to the LocalSystem service, which independently restricts the release URL, bounds and verifies the MSI, and installs without UAC or installer prompts.
- The service refuses installation while a backup, repository operation, or management operation is active and prevents new backup work while the MSI starts.
- MSI major-upgrade behavior stops and restarts the service. The update hold expires after at most 30 minutes if installation is cancelled before the service is replaced.

Rollback, interrupted-upgrade recovery, and the full Windows 10/11 repair/upgrade matrix still require release qualification before unattended updates are considered broadly production-qualified.
