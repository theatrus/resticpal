# Project instructions

## Update release invariant

Never assume that a published installer URL will be staged with an installer extension after HTTP redirects. NetSparkle selects the Windows installer command from the downloaded file's local extension; GitHub release assets can redirect to an opaque GUID path even when the response includes a correct `Content-Disposition` filename.

Before publishing any resticpal update:

1. Exercise the update using the previously published client, because new updater code cannot repair the download behavior of an already-installed old client.
2. Verify the actual staged download path ends in `.msi`, not merely that the source URL or response metadata names an MSI.
3. Run the resulting MSI through the intended prompted or LocalSystem silent installation path and confirm the version, service, and tray all upgrade and restart.
4. If the previous client cannot preserve the extension, publish a one-time direct, non-redirecting `updates.resticpal.com` MSI bridge and verify its length and SHA-256 against the signed CI artifact before signing the appcast.

Treat this as a release blocker, not a post-release check.
