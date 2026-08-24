namespace ResticPal.UI.Services;

internal enum AvailableUpdateAction
{
    WaitForSettings,
    OfferManualDownload,
    RequestAutomaticInstall,
}

internal enum AutomaticUpdateRequestAction
{
    Complete,
    RetrySilently,
}

/// <summary>
/// Keeps the no-prompt automatic-update contract explicit and independently
/// testable. If settings cannot be loaded, the UI must not guess that manual
/// mode is active because a managed policy may require silent installation.
/// </summary>
internal static class UpdateInstallationDecision
{
    internal static AvailableUpdateAction Select(
        bool settingsLoaded,
        bool automaticInstall) =>
        !settingsLoaded
            ? AvailableUpdateAction.WaitForSettings
            : automaticInstall
                ? AvailableUpdateAction.RequestAutomaticInstall
                : AvailableUpdateAction.OfferManualDownload;

    internal static AutomaticUpdateRequestAction AfterAutomaticRequest(bool accepted) =>
        accepted
            ? AutomaticUpdateRequestAction.Complete
            : AutomaticUpdateRequestAction.RetrySilently;

    internal static bool ShouldRecoverSettings(
        bool loadAttempted,
        bool settingsLoaded,
        bool settingsBusy,
        bool updateBusy,
        bool retryDue) =>
        loadAttempted && !settingsLoaded && !settingsBusy && !updateBusy && retryDue;
}
