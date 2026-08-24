using ResticPal.UI.Services;
using Xunit;

namespace ResticPal.UI.Logic.Tests;

public sealed class ConfigurationReloadGateTests
{
    [Fact]
    public void CompletedCleanPagesLeaveOnlyDirtyLikePagePending()
    {
        var plan = new ConfigurationPageSynchronizationPlan();
        plan.Begin("managed-revision-1", force: false, ConfigurationPageKind.All);

        plan.Complete(
            ConfigurationPageKind.Sources
            | ConfigurationPageKind.Schedule
            | ConfigurationPageKind.Retention
            | ConfigurationPageKind.Updates);

        Assert.Equal(ConfigurationPageKind.Repository, plan.PendingPages);
        Assert.True(plan.Needs(ConfigurationPageKind.Repository));
        Assert.False(plan.Needs(ConfigurationPageKind.Sources));
        Assert.True(plan.HasPending);
    }

    [Fact]
    public void SameRevisionRetryDoesNotRefetchCompletedPages()
    {
        var plan = new ConfigurationPageSynchronizationPlan();
        plan.Begin("managed-revision-1", force: false, ConfigurationPageKind.All);
        plan.Complete(ConfigurationPageKind.Sources | ConfigurationPageKind.Schedule);

        plan.Begin("managed-revision-1", force: false, ConfigurationPageKind.All);

        Assert.False(plan.Needs(ConfigurationPageKind.Sources));
        Assert.False(plan.Needs(ConfigurationPageKind.Schedule));
        Assert.True(plan.Needs(ConfigurationPageKind.Repository));
        Assert.True(plan.Needs(ConfigurationPageKind.Retention));
        Assert.True(plan.Needs(ConfigurationPageKind.Updates));
    }

    [Fact]
    public void PageThatBecomesEligibleDuringSameRevisionIsAddedOnce()
    {
        var plan = new ConfigurationPageSynchronizationPlan();
        plan.Begin("managed-revision-1", force: false, ConfigurationPageKind.Sources);
        plan.Complete(ConfigurationPageKind.Sources);

        plan.Begin(
            "managed-revision-1",
            force: false,
            ConfigurationPageKind.Sources | ConfigurationPageKind.Repository);

        Assert.False(plan.Needs(ConfigurationPageKind.Sources));
        Assert.True(plan.Needs(ConfigurationPageKind.Repository));
    }

    [Theory]
    [InlineData("managed-revision-2", false)]
    [InlineData("managed-revision-1", true)]
    public void NewRevisionOrForceStartsFreshPlan(string revision, bool force)
    {
        var plan = new ConfigurationPageSynchronizationPlan();
        plan.Begin("managed-revision-1", force: false, ConfigurationPageKind.All);
        plan.Complete(ConfigurationPageKind.All);
        Assert.False(plan.HasPending);

        plan.Begin(revision, force, ConfigurationPageKind.All);

        Assert.Equal(ConfigurationPageKind.All, plan.PendingPages);
        Assert.True(plan.Needs(ConfigurationPageKind.Sources));
        Assert.True(plan.Needs(ConfigurationPageKind.Repository));
        Assert.True(plan.Needs(ConfigurationPageKind.Schedule));
        Assert.True(plan.Needs(ConfigurationPageKind.Retention));
        Assert.True(plan.Needs(ConfigurationPageKind.Updates));
    }

    [Fact]
    public async Task ReloadScopeKeepsControlsDisabledAfterPageOperationEnds()
    {
        var gate = new ConfigurationEditGate();
        var reloadStarted = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var finishReload = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);

        Task reload = Task.Run(async () =>
        {
            using (gate.BeginReload())
            {
                reloadStarted.SetResult();
                await finishReload.Task;
            }
        });

        await reloadStarted.Task;
        Assert.True(gate.ControlsDisabled(operationBusy: true, baselineAvailable: true));

        // A page's own operation has ended, but another awaited page load is
        // still part of the same managed refresh.
        Assert.True(gate.ControlsDisabled(operationBusy: false, baselineAvailable: true));

        finishReload.SetResult();
        await reload;
        Assert.False(gate.ControlsDisabled(operationBusy: false, baselineAvailable: true));
    }

    [Fact]
    public void MissingBaselineKeepsControlsDisabledUntilSuccessfulLoad()
    {
        var gate = new ConfigurationEditGate();
        bool baselineAvailable = false;

        Assert.True(gate.ControlsDisabled(operationBusy: false, baselineAvailable));

        // A failed initial load leaves the baseline absent.
        Assert.True(gate.ControlsDisabled(operationBusy: false, baselineAvailable));

        baselineAvailable = true;
        Assert.False(gate.ControlsDisabled(operationBusy: false, baselineAvailable));
    }

    [Fact]
    public void FieldDiffSelectsOnlyChangedUnlockedField()
    {
        var baseline = new TestFields("daily", 5, true);
        var current = baseline with { Interval = "hourly" };

        bool intervalChanged = ConfigurationFieldDiff.Changed(
            locked: false,
            current.Interval,
            baseline.Interval,
            StringComparer.Ordinal);
        bool retryCountChanged = ConfigurationFieldDiff.Changed(
            locked: false,
            current.RetryCount,
            baseline.RetryCount);
        bool batteryChanged = ConfigurationFieldDiff.Changed(
            locked: false,
            current.AllowOnBattery,
            baseline.AllowOnBattery);

        Assert.True(intervalChanged);
        Assert.False(retryCountChanged);
        Assert.False(batteryChanged);
    }

    [Fact]
    public void FieldDiffIgnoresLockedFieldEvenWhenItsValueDiffers()
    {
        Assert.False(ConfigurationFieldDiff.Changed(
            locked: true,
            current: "changed",
            baseline: "original",
            StringComparer.Ordinal));
    }

    [Fact]
    public void OneChangedFieldOmitsEveryStaleSiblingFromUpdatePayload()
    {
        uint? interval = ConfigurationFieldDiff.ValueOrNull(
            changed: true,
            value: 12u);
        ulong? staleWakeGrace = ConfigurationFieldDiff.ValueOrNull(
            changed: false,
            value: 300ul);
        bool? staleBatteryPolicy = ConfigurationFieldDiff.ValueOrNull(
            changed: false,
            value: true);
        string[]? staleSources = ConfigurationFieldDiff.ReferenceOrNull(
            changed: false,
            value: new[] { @"C:\stale-managed-path" });

        Assert.Equal(12u, interval);
        Assert.Null(staleWakeGrace);
        Assert.Null(staleBatteryPolicy);
        Assert.Null(staleSources);
    }

    [Fact]
    public void EveryConfigurationPageCanIndependentlyDeferManagedReload()
    {
        ConfigurationPageEditState[] pageEdits =
        [
            new(Sources: true, Repository: false, Schedule: false, Retention: false),
            new(Sources: false, Repository: true, Schedule: false, Retention: false),
            new(Sources: false, Repository: false, Schedule: true, Retention: false),
            new(Sources: false, Repository: false, Schedule: false, Retention: true),
        ];

        foreach (ConfigurationPageEditState edits in pageEdits)
        {
            Assert.True(ConfigurationReloadGate.ShouldDefer(
                configurationOperationActive: false,
                edits,
                discardEditsRequested: false));
        }
    }

    [Fact]
    public void ExplicitDiscardBypassesEditsButNeverAnActiveOperation()
    {
        var edits = new ConfigurationPageEditState(
            Sources: true,
            Repository: true,
            Schedule: true,
            Retention: true);

        Assert.False(ConfigurationReloadGate.ShouldDefer(
            configurationOperationActive: false,
            edits,
            discardEditsRequested: true));
        Assert.True(ConfigurationReloadGate.ShouldDefer(
            configurationOperationActive: true,
            edits,
            discardEditsRequested: true));
    }

    [Fact]
    public void CleanIdlePagesCanReload()
    {
        Assert.False(ConfigurationReloadGate.ShouldDefer(
            configurationOperationActive: false,
            new ConfigurationPageEditState(false, false, false, false),
            discardEditsRequested: false));
    }

    private sealed record TestFields(string Interval, int RetryCount, bool AllowOnBattery);
}
