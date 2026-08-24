namespace ResticPal.UI.Services;

/// <summary>
/// Selects the status shown after a manual backup request. The acknowledgement
/// gets a short, monotonic minimum display interval so a fast service cannot
/// replace it with running or completed state before the user can perceive it.
/// </summary>
internal static class ManualBackupStatusTransition
{
    internal static readonly TimeSpan MinimumAcknowledgementDisplay =
        TimeSpan.FromSeconds(2);
    internal static readonly TimeSpan AcknowledgementTimeout = TimeSpan.FromMinutes(2);

    internal static ManualBackupStatusDecision Evaluate(
        bool requestPending,
        TimeSpan requestElapsed,
        bool attemptChanged,
        string serviceState)
    {
        if (!requestPending)
        {
            return ManualBackupStatusDecision.Authoritative;
        }

        // An accepted request must remain visible for the whole dwell even if
        // a concurrent poll has already observed running or terminal state.
        if (requestElapsed < MinimumAcknowledgementDisplay)
        {
            return ManualBackupStatusDecision.Requested;
        }

        if (requestElapsed >= AcknowledgementTimeout
            || serviceState is "unconfigured" or "paused")
        {
            return ManualBackupStatusDecision.AuthoritativeAndClear;
        }

        bool active = serviceState is "running" or "waiting";
        if (!attemptChanged && !active)
        {
            return ManualBackupStatusDecision.Requested;
        }

        return attemptChanged && !active
            ? ManualBackupStatusDecision.AuthoritativeAndClear
            : ManualBackupStatusDecision.Authoritative;
    }

    internal static TimeSpan RemainingMinimumDisplay(TimeSpan requestElapsed)
    {
        TimeSpan remaining = MinimumAcknowledgementDisplay - requestElapsed;
        return remaining > TimeSpan.Zero ? remaining : TimeSpan.Zero;
    }
}

internal readonly record struct ManualBackupStatusDecision(
    bool ShowRequested,
    bool ClearPending)
{
    internal static ManualBackupStatusDecision Requested => new(true, false);
    internal static ManualBackupStatusDecision Authoritative => new(false, false);
    internal static ManualBackupStatusDecision AuthoritativeAndClear => new(false, true);
}
