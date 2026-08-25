using ResticPal.UI.Services;
using Xunit;

namespace ResticPal.UI.Logic.Tests;

public sealed class RestorePresentationTests
{
    [Theory]
    [InlineData(null, "/")]
    [InlineData("", "/")]
    [InlineData("/", "/")]
    [InlineData("///", "/")]
    [InlineData("C/Users/Yann", "/C/Users/Yann")]
    [InlineData("/C/Users/Yann/", "/C/Users/Yann")]
    public void SnapshotPathsRemainRepositoryPaths(string? value, string expected)
    {
        Assert.Equal(expected, RestorePresentation.NormalizeSnapshotPath(value));
    }

    [Fact]
    public void BreadcrumbsRetainTheExactNavigationPathForEachSegment()
    {
        IReadOnlyList<RestoreBreadcrumb> breadcrumbs =
            RestorePresentation.Breadcrumbs("/C/Users/Yann/Documents");

        Assert.Collection(
            breadcrumbs,
            crumb => Assert.Equal(("Backup", "/"), (crumb.DisplayName, crumb.Path)),
            crumb => Assert.Equal(("C", "/C"), (crumb.DisplayName, crumb.Path)),
            crumb => Assert.Equal(("Users", "/C/Users"), (crumb.DisplayName, crumb.Path)),
            crumb => Assert.Equal(("Yann", "/C/Users/Yann"), (crumb.DisplayName, crumb.Path)),
            crumb => Assert.Equal(
                ("Documents", "/C/Users/Yann/Documents"),
                (crumb.DisplayName, crumb.Path)));
    }

    [Fact]
    public void RootHasOnlyItsFriendlyBreadcrumb()
    {
        RestoreBreadcrumb breadcrumb = Assert.Single(RestorePresentation.Breadcrumbs("/"));

        Assert.Equal("Backup", breadcrumb.DisplayName);
        Assert.Equal("/", breadcrumb.Path);
    }

    [Theory]
    [InlineData(@"C:\Users\Yann\Documents", "/C/Users/Yann/Documents", "Documents")]
    [InlineData("d:/Pictures/Trips", "/d/Pictures/Trips", "Trips")]
    [InlineData(@"C:\", "/C", "C:")]
    [InlineData(@"c:\", "/c", "c:")]
    [InlineData("c:/Documents/", "/c/Documents", "Documents")]
    public void WindowsBackupSourcesMapToExactResticSnapshotRoots(
        string source,
        string expectedPath,
        string expectedDisplayName)
    {
        Assert.True(RestorePresentation.TrySnapshotSourceRoot(
            source,
            out string snapshotPath,
            out string displayName));
        Assert.Equal(expectedPath, snapshotPath);
        Assert.Equal(expectedDisplayName, displayName);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("relative/path")]
    [InlineData("C:relative")]
    [InlineData(@"\\server\share")]
    [InlineData(@"\\?\C:\Users")]
    [InlineData(@"C:\Users\..\Secrets")]
    [InlineData(@"C:\Users\\Secrets")]
    [InlineData(@"C:\Users\.\Secrets")]
    [InlineData("C:\\Users\\bad:name")]
    [InlineData("C:\\Users\\private\nfile")]
    public void UnsupportedOrAmbiguousSourcesDoNotBecomeAuthorizedRoots(string? source)
    {
        Assert.False(RestorePresentation.TrySnapshotSourceRoot(
            source,
            out string snapshotPath,
            out string displayName));
        Assert.Empty(snapshotPath);
        Assert.Empty(displayName);
    }

    [Fact]
    public void FriendlyBreadcrumbsNeverExposeUnauthorizedParentDirectories()
    {
        IReadOnlyList<RestoreBreadcrumb> breadcrumbs = RestorePresentation.SourceBreadcrumbs(
            "/C/Users/Yann/Documents/Projects/ResticPal",
            "/C/Users/Yann/Documents",
            "Documents");

        Assert.Collection(
            breadcrumbs,
            crumb => Assert.Equal(("Backup", "/"), (crumb.DisplayName, crumb.Path)),
            crumb => Assert.Equal(
                ("Documents", "/C/Users/Yann/Documents"),
                (crumb.DisplayName, crumb.Path)),
            crumb => Assert.Equal(
                ("Projects", "/C/Users/Yann/Documents/Projects"),
                (crumb.DisplayName, crumb.Path)),
            crumb => Assert.Equal(
                ("ResticPal", "/C/Users/Yann/Documents/Projects/ResticPal"),
                (crumb.DisplayName, crumb.Path)));
    }

    [Fact]
    public void UnrelatedSourceRootsCannotRewriteNavigationBreadcrumbs()
    {
        IReadOnlyList<RestoreBreadcrumb> breadcrumbs = RestorePresentation.SourceBreadcrumbs(
            "/D/Other/file.txt",
            "/C/Documents",
            "Documents");

        Assert.Equal(["Backup", "D", "Other", "file.txt"],
            breadcrumbs.Select(crumb => crumb.DisplayName));
    }

    [Theory]
    [InlineData("running", false, "Restore in progress")]
    [InlineData("running", true, "Cancelling restore")]
    [InlineData("cancelling", false, "Cancelling restore")]
    [InlineData("cancelled", true, "Restore cancelled — partial files were kept")]
    [InlineData("succeeded", true, "Restore completed and verified")]
    [InlineData("failed", false, "Restore could not be completed")]
    public void CancellationAcknowledgementSurvivesRunningStatusPolls(
        string state,
        bool cancellationRequested,
        string expectedTitle)
    {
        Assert.Equal(expectedTitle, RestorePresentation.OperationTitle(state, cancellationRequested));
    }

    [Fact]
    public void SnapshotFilteringUsesTheAdministratorsLocalCalendarDate()
    {
        DateTimeOffset local = DateTimeOffset.Now;
        DateTimeOffset selectedDate = new(local.Date, local.Offset);
        DateTimeOffset previousDate = selectedDate.AddDays(-1);

        Assert.True(RestorePresentation.MatchesLocalDate(local, selectedDate));
        Assert.False(RestorePresentation.MatchesLocalDate(local, previousDate));
    }

    [Theory]
    [InlineData(0, "0 B")]
    [InlineData(1023, "1023 B")]
    [InlineData(1024, "1 KB")]
    [InlineData(1048576, "1 MB")]
    [InlineData(1073741824, "1 GB")]
    public void ByteFormattingUsesBinaryUnits(ulong bytes, string expected)
    {
        Assert.Equal(expected, RestorePresentation.FormatBytes(bytes));
    }

    [Fact]
    public void RestoreProgressUsesClearSingularAndPluralForms()
    {
        Assert.Equal("1 file · 1 KB restored", RestorePresentation.FormatProgress(1, 1024));
        Assert.Equal("2 files · 0 B restored", RestorePresentation.FormatProgress(2, 0));
    }

    [Fact]
    public void ServiceMessagesDoNotHideRestoreFileAndByteProgress()
    {
        Assert.Equal(
            "Restoring and verifying. · 3 files · 2 KB restored",
            RestorePresentation.FormatStatusMessage("Restoring and verifying.", 3, 2048));
        Assert.Equal(
            "Preparing a verified file restore.",
            RestorePresentation.FormatStatusMessage("Preparing a verified file restore.", 0, 0));
        Assert.Equal(
            "1 file · 1 KB restored",
            RestorePresentation.FormatStatusMessage(null, 1, 1024));
    }

    [Theory]
    [InlineData("dir", true)]
    [InlineData("directory", true)]
    [InlineData("DIRECTORY", true)]
    [InlineData("file", false)]
    [InlineData("symlink", false)]
    public void OnlyDirectoryNodesCanBeOpened(string kind, bool expected)
    {
        Assert.Equal(expected, RestorePresentation.IsDirectory(kind));
    }

    [Theory]
    [InlineData("queued", true, false)]
    [InlineData("starting", true, false)]
    [InlineData("running", true, false)]
    [InlineData("cancelling", true, false)]
    [InlineData("succeeded", false, true)]
    [InlineData("completed", false, true)]
    [InlineData("cancelled", false, true)]
    [InlineData("failed", false, true)]
    [InlineData("idle", false, false)]
    public void RestoreStatesHaveUnambiguousActivity(string state, bool active, bool terminal)
    {
        Assert.Equal(active, RestorePresentation.IsActiveState(state));
        Assert.Equal(terminal, RestorePresentation.IsTerminalState(state));
    }

    [Fact]
    public void PagingAdvancesByRowsActuallyReturnedAfterFrameSizeTrimming()
    {
        var pages = new RestorePageAccumulator<int>();

        Assert.False(pages.Add(Enumerable.Range(0, 37).ToArray(), total: 103));
        Assert.Equal(37u, pages.Offset);
        Assert.False(pages.Add(Enumerable.Range(37, 41).ToArray(), total: 103));
        Assert.Equal(78u, pages.Offset);
        Assert.True(pages.Add(Enumerable.Range(78, 25).ToArray(), total: 103));
        Assert.Equal(103u, pages.Offset);
        Assert.Equal(Enumerable.Range(0, 103), pages.Items);
    }

    [Fact]
    public void EmptyCompletedResultDoesNotRequestAnotherPage()
    {
        var pages = new RestorePageAccumulator<int>();

        Assert.True(pages.Add([], total: 0));
        Assert.Equal(0u, pages.Offset);
    }

    [Fact]
    public void EmptyPartialPageIsRejectedInsteadOfLoopingForever()
    {
        var pages = new RestorePageAccumulator<int>();

        Assert.Throws<InvalidDataException>(() => pages.Add([], total: 4));
    }

    [Fact]
    public void OversizedOrInconsistentPagesAreRejected()
    {
        var oversized = new RestorePageAccumulator<int>();
        Assert.Throws<InvalidDataException>(() =>
            oversized.Add(Enumerable.Range(0, 101).ToArray(), total: 101));

        var changedTotal = new RestorePageAccumulator<int>();
        Assert.False(changedTotal.Add([1], total: 3));
        Assert.Throws<InvalidDataException>(() => changedTotal.Add([2], total: 4));

        var overflow = new RestorePageAccumulator<int>();
        Assert.Throws<InvalidDataException>(() =>
            overflow.Add([1], total: (ulong)uint.MaxValue + 1));

        var excess = new RestorePageAccumulator<int>();
        Assert.Throws<InvalidDataException>(() => excess.Add([1, 2], total: 1));
    }
}
