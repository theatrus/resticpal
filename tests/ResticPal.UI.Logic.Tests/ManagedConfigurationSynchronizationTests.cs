using ResticPal.UI.Services;
using Xunit;

namespace ResticPal.UI.Logic.Tests;

public sealed class ManagedConfigurationSynchronizationTests
{
    [Fact]
    public async Task FirstOrdinaryObservationEstablishesBaselineWithoutReloading()
    {
        var synchronization = new ManagedConfigurationSynchronization();
        int reloads = 0;

        bool result = await synchronization.ObserveAsync("revision-1", false, (_, _) =>
        {
            reloads++;
            return Task.FromResult(true);
        });

        Assert.True(result);
        Assert.Equal(0, reloads);
        Assert.True(synchronization.HasObservedRevision);
        Assert.Equal("revision-1", synchronization.ObservedRevision);
        Assert.False(synchronization.Pending);
    }

    [Fact]
    public async Task FailedReloadKeepsRevisionPendingUntilRetrySucceeds()
    {
        var synchronization = new ManagedConfigurationSynchronization();
        await synchronization.ObserveAsync("revision-1", false, (_, _) => Task.FromResult(true));
        int reloads = 0;

        bool failed = await synchronization.ObserveAsync("revision-2", false, (_, _) =>
        {
            reloads++;
            return Task.FromResult(false);
        });

        Assert.False(failed);
        Assert.True(synchronization.Pending);
        Assert.Equal("revision-1", synchronization.ObservedRevision);

        bool retried = await synchronization.ObserveAsync("revision-2", false, (_, _) =>
        {
            reloads++;
            return Task.FromResult(true);
        });

        Assert.True(retried);
        Assert.Equal(2, reloads);
        Assert.False(synchronization.Pending);
        Assert.Equal("revision-2", synchronization.ObservedRevision);
    }

    [Fact]
    public async Task BusyConfigurationPageDefersReloadWithoutConsumingRevision()
    {
        var synchronization = new ManagedConfigurationSynchronization();
        await synchronization.ObserveAsync("revision-1", false, (_, _) => Task.FromResult(true));
        bool pageOperationActive = true;

        bool busy = await synchronization.ObserveAsync(
            "revision-2",
            false,
            (_, _) => Task.FromResult(!pageOperationActive));

        Assert.False(busy);
        Assert.True(synchronization.Pending);
        Assert.Equal("revision-1", synchronization.ObservedRevision);

        pageOperationActive = false;
        Assert.True(await synchronization.ObserveAsync(
            "revision-2",
            false,
            (_, _) => Task.FromResult(!pageOperationActive)));
        Assert.False(synchronization.Pending);
        Assert.Equal("revision-2", synchronization.ObservedRevision);
    }

    [Fact]
    public async Task UnsavedInputKeepsRevisionPendingUntilUserExplicitlyDiscardsIt()
    {
        var synchronization = new ManagedConfigurationSynchronization();
        await synchronization.ObserveAsync("revision-1", false, (_, _) => Task.FromResult(true));
        bool discardRequested = false;
        var edits = new ConfigurationPageEditState(
            Sources: false,
            Repository: true,
            Schedule: false,
            Retention: false);

        bool deferred = await synchronization.ObserveAsync("revision-2", false, (_, _) =>
            Task.FromResult(!ConfigurationReloadGate.ShouldDefer(
                configurationOperationActive: false,
                edits,
                discardRequested)));

        Assert.False(deferred);
        Assert.True(synchronization.Pending);
        Assert.Equal("revision-1", synchronization.ObservedRevision);

        discardRequested = true;
        Assert.True(await synchronization.ObserveAsync("revision-2", true, (_, _) =>
            Task.FromResult(!ConfigurationReloadGate.ShouldDefer(
                configurationOperationActive: false,
                edits,
                discardRequested))));
        Assert.False(synchronization.Pending);
        Assert.Equal("revision-2", synchronization.ObservedRevision);
    }

    [Fact]
    public async Task ForcedInitialObservationReloadsBeforeConsumingRevision()
    {
        var synchronization = new ManagedConfigurationSynchronization();
        int reloads = 0;

        bool result = await synchronization.ObserveAsync("enrolled", true, (_, _) =>
        {
            reloads++;
            return Task.FromResult(true);
        });

        Assert.True(result);
        Assert.Equal(1, reloads);
        Assert.Equal("enrolled", synchronization.ObservedRevision);
    }

    [Fact]
    public async Task ForcedPartialPassRetriesWithoutResettingCompletedPages()
    {
        var synchronization = new ManagedConfigurationSynchronization();
        await synchronization.ObserveAsync(
            "revision-1",
            false,
            (_, _) => Task.FromResult(true));
        var plan = new ConfigurationPageSynchronizationPlan();

        bool partial = await synchronization.ObserveAsync(
            "revision-2",
            true,
            (revision, force) =>
            {
                Assert.True(force);
                plan.Begin(revision, force, ConfigurationPageKind.All);
                plan.Complete(ConfigurationPageKind.Sources);
                return Task.FromResult(false);
            });

        Assert.False(partial);
        Assert.False(plan.Needs(ConfigurationPageKind.Sources));

        bool retried = await synchronization.ObserveAsync(
            "revision-2",
            false,
            (revision, force) =>
            {
                Assert.False(force);
                plan.Begin(revision, force, ConfigurationPageKind.All);
                Assert.False(plan.Needs(ConfigurationPageKind.Sources));
                plan.Complete(ConfigurationPageKind.All);
                return Task.FromResult(true);
            });

        Assert.True(retried);
        Assert.False(plan.HasPending);
        Assert.Equal("revision-2", synchronization.ObservedRevision);
    }

    [Fact]
    public async Task RevisionArrivingDuringReloadIsHandledBeforeRunnerExits()
    {
        var synchronization = new ManagedConfigurationSynchronization();
        await synchronization.ObserveAsync("revision-1", false, (_, _) => Task.FromResult(true));
        var firstReloadStarted = new TaskCompletionSource(
            TaskCreationOptions.RunContinuationsAsynchronously);
        var releaseFirstReload = new TaskCompletionSource(
            TaskCreationOptions.RunContinuationsAsynchronously);
        int reloads = 0;

        Task<bool> first = synchronization.ObserveAsync("revision-2", false, async (_, _) =>
        {
            if (Interlocked.Increment(ref reloads) == 1)
            {
                firstReloadStarted.SetResult();
                await releaseFirstReload.Task;
            }
            return true;
        });
        await firstReloadStarted.Task;

        bool queued = await synchronization.ObserveAsync(
            "revision-3",
            false,
            (_, _) => Task.FromResult(true));
        Assert.False(queued);
        releaseFirstReload.SetResult();

        Assert.True(await first);
        Assert.Equal(2, reloads);
        Assert.False(synchronization.Pending);
        Assert.Equal("revision-3", synchronization.ObservedRevision);
    }
}
