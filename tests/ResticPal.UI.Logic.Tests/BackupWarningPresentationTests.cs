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
    public void TotalCountSaturatesInsteadOfWrapping()
    {
        string summary = BackupWarningPresentation.DialogSummary(1, ulong.MaxValue);

        Assert.Contains($"{ulong.MaxValue:N0}", summary, StringComparison.Ordinal);
    }
}
