using ResticPal.UI.Services;
using Xunit;

namespace ResticPal.UI.Logic.Tests;

public sealed class BackupWarningPresentationTests
{
    [Fact]
    public void SingularAndPluralFileCountsAreClear()
    {
        Assert.Equal("View source warning details",
            BackupWarningPresentation.ViewButtonLabel(0));
        Assert.Equal("1 source item could not be read.",
            BackupWarningPresentation.CountSummary(1));
        Assert.Equal("View file restic could not back up",
            BackupWarningPresentation.ViewButtonLabel(1));
        Assert.Equal("View 2 files restic could not back up",
            BackupWarningPresentation.ViewButtonLabel(2));
    }

    [Fact]
    public void DialogExplainsLocalStorageAndOmittedPaths()
    {
        string complete = BackupWarningPresentation.DialogSummary(2, 0);
        string bounded = BackupWarningPresentation.DialogSummary(2, 3);

        Assert.Contains("2 source items", complete, StringComparison.Ordinal);
        Assert.Contains("stored only on this PC", complete, StringComparison.Ordinal);
        Assert.Contains("5 source items", bounded, StringComparison.Ordinal);
        Assert.Contains("3 additional or unsafe path entries", bounded,
            StringComparison.Ordinal);
    }

    [Fact]
    public void DialogExplainsWhenPartialWarningDetailsAreUnavailable()
    {
        string summary = BackupWarningPresentation.DialogSummary(0, 0);

        Assert.Contains("one or more source items", summary, StringComparison.Ordinal);
        Assert.Contains("paths and item count were unavailable", summary,
            StringComparison.Ordinal);
        Assert.DoesNotContain("0 source items", summary, StringComparison.Ordinal);
    }

    [Fact]
    public void TotalCountSaturatesInsteadOfWrapping()
    {
        string summary = BackupWarningPresentation.DialogSummary(1, ulong.MaxValue);

        Assert.Contains($"{ulong.MaxValue:N0}", summary, StringComparison.Ordinal);
    }

    [Fact]
    public void VssFallbackExplainsConsistencyWithoutClaimingFilesFailed()
    {
        string detail = BackupWarningPresentation.RunDetail(
            "succeeded_with_warnings",
            "restic_vss_fallback",
            0,
            "snapshot-1");

        Assert.Contains("live files", detail, StringComparison.Ordinal);
        Assert.Contains("consistently", detail, StringComparison.Ordinal);
        Assert.DoesNotContain("could not back up", detail, StringComparison.Ordinal);
        Assert.False(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "restic_vss_fallback",
            0));
    }

    [Fact]
    public void VssCleanupWarningDoesNotOfferSourceFailureDetails()
    {
        string detail = BackupWarningPresentation.RunDetail(
            "succeeded_with_warnings",
            "restic_vss_cleanup_failed",
            0,
            "snapshot-1");

        Assert.Contains("could not clean up", detail, StringComparison.Ordinal);
        Assert.Contains("Captured backup data remains usable", detail, StringComparison.Ordinal);
        Assert.False(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "restic_vss_cleanup_failed",
            0));
    }

    [Fact]
    public void OnlyPartialSourceWarningsOfferAnEmptySourceDetailView()
    {
        Assert.True(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "restic_partial_source",
            0));
        Assert.True(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "another_warning",
            2));
        Assert.False(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "another_warning",
            0));
    }

    [Fact]
    public void VssFallbackAndUnreadableItemsAreBothExplained()
    {
        string detail = BackupWarningPresentation.RunDetail(
            "succeeded_with_warnings",
            "restic_vss_fallback",
            2,
            "snapshot-1");

        Assert.Contains("live files", detail, StringComparison.Ordinal);
        Assert.Contains("2 source items could not be read", detail, StringComparison.Ordinal);
        Assert.True(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "restic_vss_fallback",
            2));
    }

    [Fact]
    public void CombinedVssWarningExplainsFallbackAndCleanup()
    {
        string detail = BackupWarningPresentation.RunDetail(
            "succeeded_with_warnings",
            "restic_vss_fallback_and_cleanup_failed",
            0,
            "snapshot-1");

        Assert.Contains("One or more sources", detail, StringComparison.Ordinal);
        Assert.Contains("live files", detail, StringComparison.Ordinal);
        Assert.Contains("could not clean up", detail, StringComparison.Ordinal);
        Assert.False(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "restic_vss_fallback_and_cleanup_failed",
            0));
    }

    [Fact]
    public void PartialSourceWithoutRetainedPathsStillExplainsTheWarning()
    {
        string detail = BackupWarningPresentation.RunDetail(
            "succeeded_with_warnings",
            "restic_partial_source",
            0,
            "snapshot-1");

        Assert.Contains("source items could not be read", detail, StringComparison.Ordinal);
        Assert.Contains("Snapshot snapshot-1", detail, StringComparison.Ordinal);
        Assert.True(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "restic_partial_source",
            0));
    }

    [Fact]
    public void PartialSourceAndCleanupWarningKeepsSourceDetailsAvailable()
    {
        string detail = BackupWarningPresentation.RunDetail(
            "succeeded_with_warnings",
            "restic_partial_source_and_vss_cleanup_failed",
            0,
            "snapshot-1");

        Assert.Contains("could not clean up", detail, StringComparison.Ordinal);
        Assert.Contains("source items could not be read", detail, StringComparison.Ordinal);
        Assert.True(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "restic_partial_source_and_vss_cleanup_failed",
            0));
    }

    [Fact]
    public void VssFallbackAndPartialSourceWarningKeepsBothExplanations()
    {
        string detail = BackupWarningPresentation.RunDetail(
            "succeeded_with_warnings",
            "restic_vss_fallback_and_partial_source",
            2,
            "snapshot-1");

        Assert.Contains("live files", detail, StringComparison.Ordinal);
        Assert.Contains("2 source items could not be read", detail, StringComparison.Ordinal);
        Assert.True(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "restic_vss_fallback_and_partial_source",
            0));
    }

    [Fact]
    public void VssFallbackPartialSourceAndCleanupWarningKeepsAllExplanations()
    {
        string detail = BackupWarningPresentation.RunDetail(
            "succeeded_with_warnings",
            "restic_vss_fallback_partial_source_and_cleanup_failed",
            2,
            "snapshot-1");

        Assert.Contains("live files", detail, StringComparison.Ordinal);
        Assert.Contains("2 source items could not be read", detail, StringComparison.Ordinal);
        Assert.Contains("could not clean up", detail, StringComparison.Ordinal);
        Assert.True(BackupWarningPresentation.HasSourceDetails(
            "succeeded_with_warnings",
            "restic_vss_fallback_partial_source_and_cleanup_failed",
            0));
    }

    [Fact]
    public void CurrentWarningStatusIsGeneric()
    {
        Assert.Contains("completed with a warning",
            BackupWarningPresentation.CurrentStatusDescription,
            StringComparison.Ordinal);
        Assert.DoesNotContain("files need attention",
            BackupWarningPresentation.CurrentStatusDescription,
            StringComparison.Ordinal);
    }
}
