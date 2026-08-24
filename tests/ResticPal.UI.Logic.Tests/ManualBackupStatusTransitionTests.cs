using ResticPal.UI.Services;
using Xunit;

namespace ResticPal.UI.Logic.Tests;

public sealed class ManualBackupStatusTransitionTests
{
    [Fact]
    public void RequestedAcknowledgementRemainsVisibleForTwoSeconds()
    {
        Assert.Equal(
            TimeSpan.FromSeconds(2),
            ManualBackupStatusTransition.MinimumAcknowledgementDisplay);
    }

    [Theory]
    [InlineData("running", false)]
    [InlineData("idle", true)]
    public void ImmediateServiceTransitionCannotHideRequestedAcknowledgement(
        string serviceState,
        bool attemptChanged)
    {
        ManualBackupStatusDecision decision = ManualBackupStatusTransition.Evaluate(
            requestPending: true,
            requestElapsed: ManualBackupStatusTransition.MinimumAcknowledgementDisplay
                - TimeSpan.FromMilliseconds(1),
            attemptChanged,
            serviceState);

        Assert.True(decision.ShowRequested);
        Assert.False(decision.ClearPending);
    }

    [Fact]
    public void TerminalAttemptBecomesAuthoritativeAtEndOfDwell()
    {
        ManualBackupStatusDecision decision = ManualBackupStatusTransition.Evaluate(
            requestPending: true,
            requestElapsed: ManualBackupStatusTransition.MinimumAcknowledgementDisplay,
            attemptChanged: true,
            serviceState: "idle");

        Assert.False(decision.ShowRequested);
        Assert.True(decision.ClearPending);
    }

    [Fact]
    public void ActiveAttemptBecomesAuthoritativeWithoutClearingPending()
    {
        ManualBackupStatusDecision decision = ManualBackupStatusTransition.Evaluate(
            requestPending: true,
            requestElapsed: ManualBackupStatusTransition.MinimumAcknowledgementDisplay,
            attemptChanged: true,
            serviceState: "running");

        Assert.False(decision.ShowRequested);
        Assert.False(decision.ClearPending);
    }

    [Fact]
    public void StalePreRequestStatusKeepsAcknowledgementAfterDwell()
    {
        ManualBackupStatusDecision decision = ManualBackupStatusTransition.Evaluate(
            requestPending: true,
            requestElapsed: ManualBackupStatusTransition.MinimumAcknowledgementDisplay
                + TimeSpan.FromMilliseconds(1),
            attemptChanged: false,
            serviceState: "idle");

        Assert.True(decision.ShowRequested);
        Assert.False(decision.ClearPending);
    }

    [Fact]
    public void RemainingDwellClampsAtZero()
    {
        Assert.Equal(
            TimeSpan.FromMilliseconds(1),
            ManualBackupStatusTransition.RemainingMinimumDisplay(
                ManualBackupStatusTransition.MinimumAcknowledgementDisplay
                    - TimeSpan.FromMilliseconds(1)));
        Assert.Equal(
            TimeSpan.Zero,
            ManualBackupStatusTransition.RemainingMinimumDisplay(
                ManualBackupStatusTransition.MinimumAcknowledgementDisplay
                    + TimeSpan.FromMilliseconds(1)));
    }

    [Theory]
    [InlineData("paused")]
    [InlineData("unconfigured")]
    public void InvalidRequestStateClearsAfterDwell(string serviceState)
    {
        ManualBackupStatusDecision decision = ManualBackupStatusTransition.Evaluate(
            requestPending: true,
            requestElapsed: ManualBackupStatusTransition.MinimumAcknowledgementDisplay,
            attemptChanged: false,
            serviceState);

        Assert.False(decision.ShowRequested);
        Assert.True(decision.ClearPending);
    }

    [Fact]
    public void UnacknowledgedRequestClearsAtTimeout()
    {
        ManualBackupStatusDecision decision = ManualBackupStatusTransition.Evaluate(
            requestPending: true,
            requestElapsed: ManualBackupStatusTransition.AcknowledgementTimeout,
            attemptChanged: false,
            serviceState: "idle");

        Assert.False(decision.ShowRequested);
        Assert.True(decision.ClearPending);
    }
}
