using ResticPal.UI.Services;
using Xunit;

namespace ResticPal.UI.Logic.Tests;

public sealed class UpdateInstallationDecisionTests
{
    [Fact]
    public void AutomaticModeRequestsProtectedServiceInstall()
    {
        Assert.Equal(
            AvailableUpdateAction.RequestAutomaticInstall,
            UpdateInstallationDecision.Select(
                settingsLoaded: true,
                automaticInstall: true));
    }

    [Fact]
    public void ManualModeOffersPromptedDownload()
    {
        Assert.Equal(
            AvailableUpdateAction.OfferManualDownload,
            UpdateInstallationDecision.Select(
                settingsLoaded: true,
                automaticInstall: false));
    }

    [Fact]
    public void UnknownEffectiveSettingNeverFallsBackToPromptedMode()
    {
        Assert.Equal(
            AvailableUpdateAction.WaitForSettings,
            UpdateInstallationDecision.Select(
                settingsLoaded: false,
                automaticInstall: false));
    }

    [Fact]
    public void AcceptedAutomaticRequestCompletesWithoutPrompting()
    {
        Assert.Equal(
            AutomaticUpdateRequestAction.Complete,
            UpdateInstallationDecision.AfterAutomaticRequest(accepted: true));
    }

    [Fact]
    public void RejectedAutomaticRequestRetriesWithoutPrompting()
    {
        Assert.Equal(
            AutomaticUpdateRequestAction.RetrySilently,
            UpdateInstallationDecision.AfterAutomaticRequest(accepted: false));
    }

    [Theory]
    [InlineData(false, false, false, false, true, false)]
    [InlineData(true, false, false, false, true, true)]
    [InlineData(true, true, false, false, true, false)]
    [InlineData(true, false, true, false, true, false)]
    [InlineData(true, false, false, true, true, false)]
    [InlineData(true, false, false, false, false, false)]
    public void SettingsRecoveryIsBoundedAndAvoidsConcurrentOperations(
        bool loadAttempted,
        bool settingsLoaded,
        bool settingsBusy,
        bool updateBusy,
        bool retryDue,
        bool expected)
    {
        Assert.Equal(
            expected,
            UpdateInstallationDecision.ShouldRecoverSettings(
                loadAttempted,
                settingsLoaded,
                settingsBusy,
                updateBusy,
                retryDue));
    }
}
