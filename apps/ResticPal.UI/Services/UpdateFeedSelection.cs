namespace ResticPal.UI.Services;

/// <summary>
/// Tracks the result of checking the ordered update feeds. A feed that says
/// the installed version is current is useful evidence, but it must not stop
/// a later fallback feed from offering a newer signed release.
/// </summary>
internal sealed class UpdateFeedSelection
{
    private bool _currentFeedSeen;

    internal UpdateFeedAction Observe(UpdateFeedObservation observation)
    {
        if (observation == UpdateFeedObservation.Current)
        {
            _currentFeedSeen = true;
        }

        return observation == UpdateFeedObservation.Available
            ? UpdateFeedAction.UseAvailableUpdate
            : UpdateFeedAction.Continue;
    }

    internal UpdateFeedCompletion Complete() => _currentFeedSeen
        ? UpdateFeedCompletion.Current
        : UpdateFeedCompletion.Unavailable;
}

internal enum UpdateFeedObservation
{
    Unavailable,
    Current,
    Available,
}

internal enum UpdateFeedAction
{
    Continue,
    UseAvailableUpdate,
}

internal enum UpdateFeedCompletion
{
    Current,
    Unavailable,
}
