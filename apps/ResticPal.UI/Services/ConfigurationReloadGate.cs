namespace ResticPal.UI.Services;

[Flags]
internal enum ConfigurationPageKind
{
    None = 0,
    Sources = 1 << 0,
    Repository = 1 << 1,
    Schedule = 1 << 2,
    Retention = 1 << 3,
    Updates = 1 << 4,
    All = Sources | Repository | Schedule | Retention | Updates,
}

/// <summary>
/// Tracks the pages that still need to observe a managed-configuration
/// revision. A retry for the same revision retains completed pages, while a
/// new revision (or an explicit force) starts a fresh synchronization pass.
/// </summary>
internal sealed class ConfigurationPageSynchronizationPlan
{
    private bool _hasTarget;
    private string? _targetRevision;
    private ConfigurationPageKind _eligiblePages;
    private ConfigurationPageKind _pendingPages;

    internal ConfigurationPageKind PendingPages => _pendingPages;

    internal bool HasPending => _pendingPages != ConfigurationPageKind.None;

    internal void Begin(
        string? targetRevision,
        bool force,
        ConfigurationPageKind eligiblePages)
    {
        eligiblePages &= ConfigurationPageKind.All;

        bool targetChanged = !_hasTarget
            || !string.Equals(_targetRevision, targetRevision, StringComparison.Ordinal);
        if (force || targetChanged)
        {
            _hasTarget = true;
            _targetRevision = targetRevision;
            _eligiblePages = eligiblePages;
            _pendingPages = eligiblePages;
            return;
        }

        // A page can become eligible after the pass starts (for example, when
        // the user first visits it). Add only genuinely new pages so pages
        // completed earlier for this revision are not fetched again.
        ConfigurationPageKind newlyEligiblePages = eligiblePages & ~_eligiblePages;
        _eligiblePages |= eligiblePages;
        _pendingPages |= newlyEligiblePages;
    }

    internal bool Needs(ConfigurationPageKind page) =>
        (_pendingPages & page & ConfigurationPageKind.All) != ConfigurationPageKind.None;

    internal void Complete(ConfigurationPageKind page)
    {
        _pendingPages &= ~(page & ConfigurationPageKind.All);
    }
}

/// <summary>
/// Coordinates a reload that can span several awaited page loads. Page-level
/// busy state may end between awaits, so the global scope remains authoritative
/// until the complete reload finishes.
/// </summary>
internal sealed class ConfigurationEditGate
{
    private int _reloadScopeCount;

    internal bool ReloadInProgress => Volatile.Read(ref _reloadScopeCount) > 0;

    internal IDisposable BeginReload()
    {
        Interlocked.Increment(ref _reloadScopeCount);
        return new ReloadScope(this);
    }

    internal bool ControlsDisabled(bool operationBusy, bool baselineAvailable) =>
        operationBusy || ReloadInProgress || !baselineAvailable;

    private void EndReload()
    {
        Interlocked.Decrement(ref _reloadScopeCount);
    }

    private sealed class ReloadScope(ConfigurationEditGate owner) : IDisposable
    {
        private ConfigurationEditGate? _owner = owner;

        public void Dispose()
        {
            ConfigurationEditGate? ownerToRelease = Interlocked.Exchange(ref _owner, null);
            ownerToRelease?.EndReload();
        }
    }
}

internal static class ConfigurationFieldDiff
{
    internal static bool Changed<T>(
        bool locked,
        T current,
        T baseline,
        IEqualityComparer<T>? comparer = null) =>
        !locked && !(comparer ?? EqualityComparer<T>.Default).Equals(current, baseline);

    internal static T? ValueOrNull<T>(bool changed, T value)
        where T : struct =>
        changed ? value : null;

    internal static T? ReferenceOrNull<T>(bool changed, T value)
        where T : class =>
        changed ? value : null;
}

/// <summary>
/// Describes local edits that a managed-policy reload must not overwrite.
/// Keeping the editable configuration surfaces explicit makes adding a new page a
/// deliberate change to both the guard and its tests.
/// </summary>
internal readonly record struct ConfigurationPageEditState(
    bool Sources,
    bool Repository,
    bool Schedule,
    bool Retention)
{
    internal bool HasUnsavedChanges => Sources || Repository || Schedule || Retention;
}

internal static class ConfigurationReloadGate
{
    internal static bool ShouldDefer(
        bool configurationOperationActive,
        ConfigurationPageEditState edits,
        bool discardEditsRequested) =>
        configurationOperationActive || (edits.HasUnsavedChanges && !discardEditsRequested);
}
