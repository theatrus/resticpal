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

The one-time v1.0.7 transition uses a bridge-first two-phase publication. Clients through v1.0.6 read the legacy `appcast.xml`; every v1.0.7-and-later client reads `appcast-v2.xml`. The same canonical, signed XML bytes are published under both names so both client generations see the same reviewed enclosure only after it has passed both previous-client update paths. `Publish-Release.ps1` deliberately rejects versions other than 1.0.7 until a later release defines whether to retire the legacy channel or requalify the oldest supported legacy client.

The first phase publishes exactly the package assets: the signed MSI, its stage checksum, license, and notices. It publishes no appcast or detached appcast signature. The deployed legacy release hook treats that missing feed as an intentional package-only bridge: it copies the MSI to the candidate's direct, non-redirecting updates-host URL and leaves the currently live legacy feed untouched. This creates the URL that old NetSparkle clients need without exposing the unqualified candidate:

```powershell
.\scripts\Publish-Release.ps1 `
    -ReleaseNotesPath C:\path\to\release-notes.md `
    -Stage
```

`-Stage` requires a clean checkout at the exact `origin/main` commit. It selects only a successful manual or matching version-tag `Windows CI` run for that commit and verifies the StackFoundry LLC Authenticode identity. It first creates a draft release whose hidden body marker records the exact signed CI run, then separately uploads and byte-validates only the MSI, checksum, license, and notices. Only after that complete package-only stage is present does the script publish it as stable/latest and trigger the MSI mirror. Any appcast asset in a staged release is rejected. The MSI asset label also records the signed run ID; every later preparation and finalization reuses that run and verifies the remote MSI digest. Interrupted staging is repairable only from known, byte-identical package assets and labels.

The mirror copies and validates the candidate MSI without advancing the legacy feed because no `appcast.xml` asset exists yet. The direct URL's no-redirect length and SHA-256 verification below is authoritative for proceeding. Do not generate, sign, or upload either appcast name before that direct package check passes.

Wait for the release hook to expose `https://updates.resticpal.com/releases/v<version>/resticpal-<version>-x64.msi`. A `HEAD` and a full `GET` must return HTTP 200 without a redirect, and the full response must match the signed CI artifact's length and SHA-256.

Now prepare the signed feed locally. `UpdatesHost` is the default and the script repeats the no-redirect, length, and SHA-256 comparison before it reads the private update key:

```powershell
.\scripts\Publish-Release.ps1
```

Preparation downloads the same validated CI artifact, confirms the backed-up public key matches the key compiled into resticpal, and generates and verifies the canonical `feed/appcast-v2.xml` plus its detached signature. It then creates byte-identical `feed/appcast.xml` and `feed/appcast.xml.signature` aliases; no second signing operation or independently generated legacy document is allowed. It writes final checksums and a schema-5 `artifacts/release/v<version>/release-manifest.json` whose `dual_named_feed` record binds both names, their byte identity, the candidate enclosure, and the qualification strategy. For the one-time v1.0.6→v1.0.7 transition it also generates `probe/appcast-v2-probe.xml`, its production-key signature, and a tiny v1.0.8 sentinel payload whose enclosure signature is deliberately invalid. Probe files are qualification inputs and are never release assets.

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

Finalization also requires the immediately preceding stable GitHub MSI, candidate hashes, actual `.msi` staging, upgraded file versions, restarted LocalSystem service, and one replacement tray. It revalidates the schema-5 manifest, signed MSI, both names of the byte-identical signed feed, checksum set, source/run/tag identity, and a fresh no-redirect mirror download. Only then does it bind both evidence snapshots and upload the reviewed files in rollout order: final checksum, v2 signature, v2 XML, legacy signature, and `appcast.xml` last. Because the staged release is already stable/latest, that final upload makes the candidate visible through the legacy GitHub fallback immediately. The subsequent release edit triggers the deployed hook to copy the same pair to the live legacy updates-host endpoint.

The qualification evidence loader reads each result once while denying writers, then parses and hashes that same byte snapshot. `Test-VersionConsistency.ps1` runs the table-driven `Test-ReleaseQualification.ps1` suite in CI, including malformed schemas and types, missing or partial evidence, swapped modes, duplicate result files, and partial or mismatched manifest bindings.

After finalization, require the live `https://updates.resticpal.com/appcast.xml` pair and both GitHub release name pairs to be byte-identical to the reviewed files, verify the strict Ed25519 signature again, and confirm the official MSI still returns a direct HTTP 200 with the reviewed length and SHA-256. The currently deployed legacy hook does not mirror `appcast-v2.xml`, so `https://updates.resticpal.com/appcast-v2.xml` is expected to remain HTTP 404 during this release. v1.0.7-and-later clients try that primary URL first, then successfully consume the identical signed pair from `https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml`. Clients through v1.0.6 consume the newly advanced live legacy pair; this intentionally includes any still-installed older client that reads the same legacy channel. Upgrading the host helper to mirror the v2 name later improves primary availability but is not required for correctness or this release.

### Why every release keeps the direct package URL

Versions 1.0.3 and 1.0.4 let NetSparkle derive their temporary filename after following redirects. A GitHub release asset can redirect to an opaque object path, so a valid MSI may be downloaded under an extensionless GUID and never reach `msiexec`. Version 1.0.5 explicitly assigns a `.msi` staging name, but older installed clients remain eligible for newer updates. Consequently, every current appcast continues to use the direct `updates.resticpal.com` package URL; switching the enclosure back to GitHub would regress those clients and is rejected by the deployment mirror.

`New-UpdateAppcast.ps1` also defaults to `UpdatesHost`. Its explicit `-PackageHost GitHub` option is retained only for isolated compatibility testing; the staged production flow rejects it. New clients accept only the exact versioned GitHub or official updates-host paths and still require the pinned Ed25519 package signature.

## Safety properties

- The private update key is read only by the local release process and never printed or uploaded.
- Staging carries only exact package assets and rejects every appcast asset, so the legacy hook can establish the direct MSI bridge without advancing a feed.
- Preparation signs one canonical v2 document and creates byte-identical legacy-name aliases; the schema-5 `dual_named_feed` manifest prevents either pair from drifting.
- Finalization is mechanically blocked until a passing result from the official immediately preceding client is bound to the exact prepared MSI and appcast hashes.
- Finalization uploads the checksum and complete v2 pair first, then the legacy signature and `appcast.xml` last; that last GitHub upload is the rollout point, and the following release edit advances the primary legacy mirror.
- Operators must hold an exclusive release window: no other stable release may be published during staging or finalization because the deployed hook resolves GitHub's latest release rather than the webhook tag. The script repeatedly checks the actual latest release and never forces `--latest`, but GitHub provides no cross-process transaction around local publication.
- Appcast generation refuses an unsigned MSI, an unexpected Authenticode publisher, a mismatched product version, or a Dropbox public key that differs from the compiled public key.
- NetSparkle runs in strict Ed25519 mode and verifies the appcast and downloaded MSI.
- NetSparkle is given an explicit unique `.msi` download filename because redirected release assets may otherwise be staged without an extension; Windows Installer selection depends on that suffix.

The deployed legacy helper has one accepted recovery limitation: it moves the live XML before its detached signature and short-circuits future runs when only the XML version is current. An interruption between those moves can leave the primary pair mismatched and cannot be repaired by rerunning this release script. Finalization waits up to 20 minutes and fails loudly if that occurs; clients can still use the exact GitHub fallback, but restoring the primary then requires an authorized host repair or a subsequent point release. Deploying the newer exact-byte helper removes this residual window.
- Prompted installs use the elevated WinUI application and quiet MSI arguments. When an administrator enables automatic installation, the tray sends the signed enclosure metadata to the LocalSystem service, which independently restricts the release URL, bounds and verifies the MSI, and installs without UAC or installer prompts.
- The service refuses installation while a backup, repository operation, or management operation is active and prevents new backup work while the MSI starts.
- MSI major-upgrade behavior stops and restarts the service. The update hold expires after at most 30 minutes if installation is cancelled before the service is replaced.

Rollback, interrupted-upgrade recovery, and the full Windows 10/11 repair/upgrade matrix still require release qualification before unattended updates are considered broadly production-qualified.
