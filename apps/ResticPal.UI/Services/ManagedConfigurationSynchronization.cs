namespace ResticPal.UI.Services;

/// <summary>
/// Coordinates policy-revision observations with configuration-page reloads.
/// A revision is consumed only after its reload succeeds. Requests received
/// while a reload is active remain pending and are serviced by the active
/// runner before it exits.
/// </summary>
internal sealed class ManagedConfigurationSynchronization
{
    private readonly object _gate = new();
    private bool _hasObservedRevision;
    private string? _observedRevision;
    private bool _synchronizationInProgress;
    private bool _synchronizationPending;
    private string? _pendingRevision;
    private bool _pendingForceSynchronization;

    internal bool Pending
    {
        get
        {
            lock (_gate)
            {
                return _synchronizationPending;
            }
        }
    }

    internal bool HasObservedRevision
    {
        get
        {
            lock (_gate)
            {
                return _hasObservedRevision;
            }
        }
    }

    internal string? ObservedRevision
    {
        get
        {
            lock (_gate)
            {
                return _observedRevision;
            }
        }
    }

    internal async Task<bool> ObserveAsync(
        string? revision,
        bool forceSynchronization,
        Func<string?, bool, Task<bool>> synchronize)
    {
        ArgumentNullException.ThrowIfNull(synchronize);

        lock (_gate)
        {
            // The first ordinary status read establishes a baseline. A forced
            // read (enrollment/unenrollment) must reload even when it is first.
            if (!_hasObservedRevision
                && !forceSynchronization
                && !_synchronizationPending
                && !_synchronizationInProgress)
            {
                _observedRevision = revision;
                _hasObservedRevision = true;
                return true;
            }

            bool revisionChanged = !_hasObservedRevision
                || !string.Equals(_observedRevision, revision, StringComparison.Ordinal);
            if (!forceSynchronization && !revisionChanged && !_synchronizationPending)
            {
                return true;
            }

            _pendingRevision = revision;
            _pendingForceSynchronization |= forceSynchronization;
            _synchronizationPending = true;
            if (_synchronizationInProgress)
            {
                return false;
            }
            _synchronizationInProgress = true;
        }

        while (true)
        {
            string? targetRevision;
            bool targetForceSynchronization;
            lock (_gate)
            {
                if (!_synchronizationPending)
                {
                    _synchronizationInProgress = false;
                    return true;
                }
                targetRevision = _pendingRevision;
                targetForceSynchronization = _pendingForceSynchronization;
                _synchronizationPending = false;
                _pendingForceSynchronization = false;
            }

            bool synchronized;
            try
            {
                synchronized = await synchronize(targetRevision, targetForceSynchronization);
            }
            catch
            {
                PreserveFailedTarget(targetRevision, targetForceSynchronization);
                throw;
            }

            if (!synchronized)
            {
                // The callback has observed the force flag and can establish
                // its per-target reload plan before returning false. Retrying
                // the same forced target as another forced pass would reset
                // already completed pages on every status poll.
                PreserveFailedTarget(targetRevision, targetForceSynchronization: false);
                return false;
            }

            lock (_gate)
            {
                _observedRevision = targetRevision;
                _hasObservedRevision = true;
            }
        }
    }

    private void PreserveFailedTarget(string? targetRevision, bool targetForceSynchronization)
    {
        lock (_gate)
        {
            // Do not overwrite a newer revision queued while the failed load
            // was in flight.
            if (!_synchronizationPending)
            {
                _pendingRevision = targetRevision;
                _pendingForceSynchronization = targetForceSynchronization;
                _synchronizationPending = true;
            }
            _synchronizationInProgress = false;
        }
    }
}
