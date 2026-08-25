using ResticPal.UI.Services;
using Xunit;

namespace ResticPal.UI.Logic.Tests;

public sealed class ServicePipeIdentityTests
{
    [Theory]
    [InlineData(1u)]
    [InlineData(42u)]
    [InlineData(uint.MaxValue)]
    public void AcceptsOnlyTheExactRunningServiceProcess(uint processId)
    {
        Assert.True(ServicePipeIdentity.MatchesRunningService(
            processId,
            ServicePipeIdentity.ServiceRunning,
            processId));
    }

    [Theory]
    [InlineData(0u, 4u, 0u)]
    [InlineData(0u, 4u, 42u)]
    [InlineData(42u, 4u, 0u)]
    [InlineData(42u, 4u, 43u)]
    [InlineData(42u, 0u, 42u)]
    [InlineData(42u, 1u, 42u)]
    [InlineData(42u, 2u, 42u)]
    [InlineData(42u, 3u, 42u)]
    [InlineData(42u, 5u, 42u)]
    [InlineData(42u, 6u, 42u)]
    [InlineData(42u, 7u, 42u)]
    public void RejectsUnknownMismatchedOrNonrunningServiceProcesses(
        uint pipeProcessId,
        uint serviceState,
        uint serviceProcessId)
    {
        Assert.False(ServicePipeIdentity.MatchesRunningService(
            pipeProcessId,
            serviceState,
            serviceProcessId));
    }
}
