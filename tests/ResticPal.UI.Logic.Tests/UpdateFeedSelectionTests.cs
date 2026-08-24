using ResticPal.UI.Services;
using Xunit;

namespace ResticPal.UI.Logic.Tests;

public sealed class UpdateFeedSelectionTests
{
    [Fact]
    public void CurrentPrimaryDoesNotHideAvailableFallback()
    {
        var selection = new UpdateFeedSelection();

        Assert.Equal(
            UpdateFeedAction.Continue,
            selection.Observe(UpdateFeedObservation.Current));
        Assert.Equal(
            UpdateFeedAction.UseAvailableUpdate,
            selection.Observe(UpdateFeedObservation.Available));
    }

    [Fact]
    public void CurrentResultWinsOnlyAfterEveryFeedWasChecked()
    {
        var selection = new UpdateFeedSelection();

        selection.Observe(UpdateFeedObservation.Current);
        selection.Observe(UpdateFeedObservation.Unavailable);

        Assert.Equal(UpdateFeedCompletion.Current, selection.Complete());
    }

    [Fact]
    public void AllUnavailableFeedsProduceUnavailableResult()
    {
        var selection = new UpdateFeedSelection();

        selection.Observe(UpdateFeedObservation.Unavailable);
        selection.Observe(UpdateFeedObservation.Unavailable);

        Assert.Equal(UpdateFeedCompletion.Unavailable, selection.Complete());
    }
}
