namespace ResticPal.UI.Services;

/// <summary>Stable, testable copy for backup warnings and local source-failure details.</summary>
internal static class BackupWarningPresentation
{
    public const string CurrentStatusDescription =
        "The latest backup completed with a warning. Review backup history for details.";

    public static string CountSummary(ulong count) => count == 1
        ? "1 source item could not be read."
        : $"{count:N0} source items could not be read.";

    public static string ViewButtonLabel(ulong count) => count switch
    {
        0 => "View source warning details",
        1 => "View file restic could not back up",
        _ => $"View {count:N0} files restic could not back up",
    };

    public static string DialogSummary(int retained, ulong omitted)
    {
        ulong retainedCount = (ulong)Math.Max(0, retained);
        if (retainedCount == 0 && omitted == 0)
        {
            return "restic reported that one or more source items could not be read, but the affected paths and item count were unavailable.";
        }

        ulong total = ulong.MaxValue - retainedCount < omitted
            ? ulong.MaxValue
            : retainedCount + omitted;
        string shown = total == 1
            ? "restic reported one source item it could not read."
            : $"restic reported {total:N0} source items it could not read.";
        return omitted == 0
            ? $"{shown} These paths are stored only on this PC."
            : $"{shown} {omitted:N0} additional or unsafe path entries were omitted by the local safety limit.";
    }

    public static string RunDetail(
        string outcome,
        string? errorCode,
        ulong failedItemCount,
        string? snapshotId)
    {
        bool hasFallback = HasVssFallback(errorCode);
        bool hasPartialSource = HasPartialSource(errorCode);
        bool hasCleanupFailure = HasVssCleanupFailure(errorCode);
        var knownWarnings = new List<string>();
        if (hasFallback)
        {
            knownWarnings.Add(
                "One or more sources could not use a Windows filesystem snapshot and were backed up from live files, so files changing during the run may not be captured consistently.");
        }
        if (hasPartialSource && failedItemCount == 0)
        {
            knownWarnings.Add(
                "One or more source items could not be read. Their paths were unavailable or omitted by the local safety limit.");
        }
        if (hasCleanupFailure)
        {
            knownWarnings.Add(
                "Windows could not clean up one or more filesystem snapshots afterward. Captured backup data remains usable, but a stale snapshot may need attention.");
        }
        string? knownWarning = knownWarnings.Count == 0
            ? null
            : string.Join(" ", knownWarnings);

        if (failedItemCount > 0)
        {
            string sourceDetail = CountSummary(failedItemCount);
            string detail = knownWarning is null
                ? sourceDetail
                : $"{knownWarning} {sourceDetail}";
            return AppendSnapshot(detail, snapshotId);
        }

        if (knownWarning is not null)
        {
            return AppendSnapshot(knownWarning, snapshotId);
        }
        if (!string.IsNullOrWhiteSpace(errorCode))
        {
            string detail = outcome == "succeeded_with_warnings"
                ? $"Backup warning code: {errorCode}"
                : $"Sanitized error code: {errorCode}";
            return AppendSnapshot(detail, snapshotId);
        }
        return !string.IsNullOrWhiteSpace(snapshotId)
            ? $"Snapshot {snapshotId}"
            : "No additional details.";
    }

    public static bool HasSourceDetails(
        string outcome,
        string? errorCode,
        ulong failedItemCount) =>
        failedItemCount > 0
        || (outcome == "succeeded_with_warnings" && HasPartialSource(errorCode));

    private static bool HasVssFallback(string? errorCode) =>
        errorCode is "restic_vss_fallback"
            or "restic_vss_fallback_and_cleanup_failed"
            or "restic_vss_fallback_and_partial_source"
            or "restic_vss_fallback_partial_source_and_cleanup_failed";

    private static bool HasPartialSource(string? errorCode) =>
        errorCode is "restic_partial_source"
            or "restic_partial_source_and_vss_cleanup_failed"
            or "restic_vss_fallback_and_partial_source"
            or "restic_vss_fallback_partial_source_and_cleanup_failed";

    private static bool HasVssCleanupFailure(string? errorCode) =>
        errorCode is "restic_vss_cleanup_failed"
            or "restic_vss_fallback_and_cleanup_failed"
            or "restic_partial_source_and_vss_cleanup_failed"
            or "restic_vss_fallback_partial_source_and_cleanup_failed";

    private static string AppendSnapshot(string detail, string? snapshotId) =>
        string.IsNullOrWhiteSpace(snapshotId)
            ? detail
            : $"{detail} Snapshot {snapshotId}.";
}
