using System.Diagnostics;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>
/// Shell of the settings window: navigation, overview actions, shared status,
/// and the guarded dispatch helper. Each page's handlers and state live in a
/// MainWindow.*.cs partial alongside this file.
/// </summary>
public sealed partial class MainWindow : Window
{
    private const string ConfigurationRefreshDeferredMessage =
        "Managed settings changed, but resticpal kept your unsaved configuration and credential inputs. Save them, or discard them to refresh the managed values.";
    private readonly ResticPalServiceClient _service = new();
    private readonly ResticPalUpdateService _updates = new();
    private readonly ManagedConfigurationSynchronization _managedConfigurationSynchronization =
        new();
    private readonly ConfigurationPageSynchronizationPlan _configurationSynchronizationPlan =
        new();
    private readonly ConfigurationEditGate _configurationEditGate = new();
    private readonly bool _showOnboarding;
    private readonly bool _showUpdates;
    private readonly HashSet<string> _activeOperations = new(StringComparer.Ordinal);
    private readonly DispatcherTimer _statusRefreshTimer = new();
    private bool _statusRefreshInProgress;
    private bool _discardConfigurationEditsRequested;
    private bool _configurationRefreshDeferredForEdits;
    private bool _manualBackupPending;
    private long? _manualBackupRequestedTimestamp;
    private DateTimeOffset? _manualBackupBaselineAttempt;
    private ServiceSnapshot? _lastServiceSnapshot;

    public MainWindow(bool showOnboarding = false, bool showUpdates = false)
    {
        _showOnboarding = showOnboarding;
        _showUpdates = showUpdates;
        InitializeComponent();
        BackupPaths.CollectionChanged += (_, _) => RefreshSourcesControlState();
        RefreshConfigurationControlStates();
        AppWindow.SetIcon(Path.Combine(AppContext.BaseDirectory, "resticpal.ico"));
        _statusRefreshTimer.Interval = TimeSpan.FromSeconds(15);
        _statusRefreshTimer.Tick += StatusRefreshTimer_Tick;
        Closed += (_, _) =>
        {
            _statusRefreshTimer.Stop();
            _updates.Dispose();
        };
        UpdateStatusDescription.Text =
            $"resticpal {ResticPalUpdateService.InstalledVersion} uses a strictly signed update feed.";
    }

    private async void NavigationView_Loaded(object sender, RoutedEventArgs e)
    {
        ServiceSnapshot? status = await RefreshStatusAsync();
        _statusRefreshTimer.Start();
        if (_showOnboarding || status?.State == "unconfigured")
        {
            ShowOnboarding();
        }
        if (_showUpdates)
        {
            ShowUpdates();
        }
        await LoadUpdateSettingsAsync();
        await CheckForUpdatesAsync(userInitiated: false);
        if (_showUpdates)
        {
            await Task.Delay(100);
            ScrollUpdatesIntoView();
        }
    }

    private async void NavigationView_SelectionChanged(
        NavigationView sender,
        NavigationViewSelectionChangedEventArgs args)
    {
        string tag = args.IsSettingsSelected
            ? "settings"
            : (args.SelectedItemContainer?.Tag as string) ?? "overview";
        OverviewPanel.Visibility = tag == "overview" ? Visibility.Visible : Visibility.Collapsed;
        SourcesPanel.Visibility = tag == "sources" ? Visibility.Visible : Visibility.Collapsed;
        RepositoryPanel.Visibility = tag == "repository" ? Visibility.Visible : Visibility.Collapsed;
        SchedulePanel.Visibility = tag == "schedule" ? Visibility.Visible : Visibility.Collapsed;
        RetentionPanel.Visibility = tag == "retention" ? Visibility.Visible : Visibility.Collapsed;
        HistoryPanel.Visibility = tag == "history" ? Visibility.Visible : Visibility.Collapsed;
        DiagnosticsPanel.Visibility = tag == "diagnostics" ? Visibility.Visible : Visibility.Collapsed;
        ManagementPanel.Visibility = tag == "settings" ? Visibility.Visible : Visibility.Collapsed;

        if (tag == "overview")
        {
            await RefreshStatusAsync();
        }
        else if (tag == "sources" && !_sourcesLoaded)
        {
            await LoadBackupSourcesAsync();
        }
        else if (tag == "repository" && !_repositoryLoaded)
        {
            await LoadRepositoryAsync();
        }
        else if (tag == "schedule" && !_scheduleLoaded)
        {
            await LoadScheduleAsync();
        }
        else if (tag == "retention" && !_retentionLoaded)
        {
            await LoadRetentionAsync();
        }
        else if (tag == "history" && !_historyLoaded)
        {
            await LoadHistoryAsync();
        }
        else if (tag == "diagnostics" && !_diagnosticsLoaded)
        {
            await LoadDiagnosticsAsync();
        }
        else if (tag == "settings" && !_managementLoaded)
        {
            await LoadManagementAsync();
        }
    }

    private async void RunBackupButton_Click(object sender, RoutedEventArgs e)
    {
        RunBackupButton.IsEnabled = false;
        try
        {
            DateTimeOffset? baselineAttempt = _lastServiceSnapshot?.LastAttempt;
            CommandResult result = await _service.RunBackupNowAsync();
            if (!result.Accepted)
            {
                ShowMessage(InfoBarSeverity.Warning, result.Message);
            }
            else
            {
                _manualBackupPending = true;
                _manualBackupRequestedTimestamp = Stopwatch.GetTimestamp();
                _manualBackupBaselineAttempt = baselineAttempt;
                ApplyStatus(
                    "Backup requested",
                    "The service accepted the request and is preparing to start the backup.",
                    canRunBackup: false,
                    canCancelBackup: false);
                UpdateStatusRefreshCadence();
                _statusRefreshTimer.Start();
                TimeSpan remainingDisplay;
                while ((remainingDisplay =
                        ManualBackupStatusTransition.RemainingMinimumDisplay(
                            ManualBackupRequestElapsed())) > TimeSpan.Zero)
                {
                    await Task.Delay(remainingDisplay);
                }
            }
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            await RefreshStatusAsync();
        }
    }

    private async void CancelBackupButton_Click(object sender, RoutedEventArgs e)
    {
        CancelBackupButton.IsEnabled = false;
        try
        {
            CommandResult result = await _service.CancelBackupAsync();
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            await RefreshStatusAsync();
        }
    }

    private void ViewWarningDetailsButton_Click(object sender, RoutedEventArgs e)
    {
        _historyLoaded = false;
        NavigationRoot.SelectedItem = HistoryItem;
    }

    /// <summary>
    /// Runs one page operation with an optional busy indicator, surfacing any
    /// failure in the shared message bar. Starting an operation that is already
    /// in flight is ignored, which keeps rapid navigation and double
    /// activations single-flight without per-page bookkeeping.
    /// </summary>
    private async Task RunGuardedAsync(string operation, Func<Task> action, Action<bool>? setBusy = null)
    {
        if (!_activeOperations.Add(operation))
        {
            return;
        }
        setBusy?.Invoke(true);
        try
        {
            await action();
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            _activeOperations.Remove(operation);
            setBusy?.Invoke(false);
        }
    }

    /// <summary>
    /// Invalidates every view backed by effective configuration and eagerly
    /// reloads the ones the administrator has already visited. Enrollment,
    /// unenrollment, and background policy revisions can replace several
    /// independently locked fields at once; keeping old page caches would
    /// expose stale values and edit controls.
    /// </summary>
    private async Task<bool> SynchronizeConfigurationPagesCoreAsync(
        string? targetRevision,
        bool forceSynchronization)
    {
        ConfigurationPageKind eligiblePages = EligibleConfigurationPages();
        _configurationSynchronizationPlan.Begin(
            targetRevision,
            forceSynchronization,
            eligiblePages);

        // A save may have loaded the pending page before its final status
        // refresh. In that case there is nothing left to race, even though the
        // save operation remains active until RunGuardedAsync unwinds.
        if (!_configurationSynchronizationPlan.HasPending)
        {
            ClearConfigurationRefreshDeferred();
            return true;
        }

        // A load/save already in flight may have captured the previous policy.
        // Let it finish and retry on the next status poll. The plan retains
        // already refreshed pages, so a busy or dirty page never causes clean
        // siblings to be fetched repeatedly.
        if (ConfigurationPageOperationActive())
        {
            return false;
        }

        bool discardEdits = _discardConfigurationEditsRequested;
        bool reloadAnyPage = PendingConfigurationPagesThatCanReload(discardEdits)
            != ConfigurationPageKind.None;
        if (reloadAnyPage)
        {
            IDisposable reloadScope = _configurationEditGate.BeginReload();
            RefreshConfigurationControlStates();
            try
            {
                await ReloadPendingConfigurationPageAsync(
                    ConfigurationPageKind.Sources,
                    SourcesHaveUnsavedChanges(),
                    discardEdits,
                    async () =>
                    {
                        _sourcesLoaded = false;
                        await LoadBackupSourcesAsync();
                        return _sourcesLoaded;
                    });
                await ReloadPendingConfigurationPageAsync(
                    ConfigurationPageKind.Repository,
                    RepositoryHasUnsavedChanges(),
                    discardEdits,
                    async () =>
                    {
                        _repositoryLoaded = false;
                        await LoadRepositoryAsync();
                        return _repositoryLoaded;
                    });
                await ReloadPendingConfigurationPageAsync(
                    ConfigurationPageKind.Schedule,
                    ScheduleHasUnsavedChanges(),
                    discardEdits,
                    async () =>
                    {
                        _scheduleLoaded = false;
                        await LoadScheduleAsync();
                        return _scheduleLoaded;
                    });
                await ReloadPendingConfigurationPageAsync(
                    ConfigurationPageKind.Retention,
                    RetentionHasUnsavedChanges(),
                    discardEdits,
                    async () =>
                    {
                        _retentionLoaded = false;
                        await LoadRetentionAsync();
                        return _retentionLoaded;
                    });
                await ReloadPendingConfigurationPageAsync(
                    ConfigurationPageKind.Updates,
                    hasUnsavedChanges: false,
                    discardEdits: discardEdits,
                    async () =>
                    {
                        _updateSettingsLoaded = false;
                        await LoadUpdateSettingsAsync();
                        return _updateSettingsLoaded;
                    });
            }
            finally
            {
                reloadScope.Dispose();
                // Each inner RunGuardedAsync finishes while the outer reload
                // scope is active, so its Set*Busy(false) deliberately leaves
                // controls disabled. Recompute after disposal to restore the
                // baseline/lock-aware enabled state.
                RefreshConfigurationControlStates();
            }
        }

        ConfigurationPageEditState remainingEdits = CurrentConfigurationPageEdits();
        if (_configurationSynchronizationPlan.HasPending
            && remainingEdits.HasUnsavedChanges
            && !discardEdits)
        {
            ShowConfigurationRefreshDeferred();
        }
        else if (!_configurationSynchronizationPlan.HasPending)
        {
            ClearConfigurationRefreshDeferred();
        }

        return !_configurationSynchronizationPlan.HasPending;
    }

    private ConfigurationPageKind EligibleConfigurationPages()
    {
        ConfigurationPageKind pages = ConfigurationPageKind.None;
        // Update policy controls whether an ordinary-user tray may authorize a
        // LocalSystem MSI. Keep it synchronized even when Settings is not the
        // currently visible page.
        pages |= ConfigurationPageKind.Updates;
        if (_loadedBackupSources is not null || SourcesPanel.Visibility == Visibility.Visible)
        {
            pages |= ConfigurationPageKind.Sources;
        }
        if (_repositoryConfigurationApplied
            || RepositoryPanel.Visibility == Visibility.Visible)
        {
            pages |= ConfigurationPageKind.Repository;
        }
        if (_loadedScheduleConfiguration is not null
            || SchedulePanel.Visibility == Visibility.Visible)
        {
            pages |= ConfigurationPageKind.Schedule;
        }
        if (_loadedRetentionConfiguration is not null
            || RetentionPanel.Visibility == Visibility.Visible)
        {
            pages |= ConfigurationPageKind.Retention;
        }
        return pages;
    }

    private ConfigurationPageKind PendingConfigurationPagesThatCanReload(bool discardEdits)
    {
        ConfigurationPageKind pages = ConfigurationPageKind.None;
        AddIfReloadable(
            ConfigurationPageKind.Sources,
            SourcesHaveUnsavedChanges());
        AddIfReloadable(
            ConfigurationPageKind.Repository,
            RepositoryHasUnsavedChanges());
        AddIfReloadable(
            ConfigurationPageKind.Schedule,
            ScheduleHasUnsavedChanges());
        AddIfReloadable(
            ConfigurationPageKind.Retention,
            RetentionHasUnsavedChanges());
        AddIfReloadable(
            ConfigurationPageKind.Updates,
            hasUnsavedChanges: false);
        return pages;

        void AddIfReloadable(ConfigurationPageKind page, bool hasUnsavedChanges)
        {
            if (_configurationSynchronizationPlan.Needs(page)
                && (discardEdits || !hasUnsavedChanges))
            {
                pages |= page;
            }
        }
    }

    private async Task ReloadPendingConfigurationPageAsync(
        ConfigurationPageKind page,
        bool hasUnsavedChanges,
        bool discardEdits,
        Func<Task<bool>> reload)
    {
        if (!_configurationSynchronizationPlan.Needs(page)
            || hasUnsavedChanges && !discardEdits)
        {
            return;
        }

        if (await reload())
        {
            _configurationSynchronizationPlan.Complete(page);
        }
    }

    private bool ConfigurationPageOperationActive() =>
        ConfigurationPageOperationActive("sources-")
        || ConfigurationPageOperationActive("repository-")
        || ConfigurationPageOperationActive("schedule-")
        || ConfigurationPageOperationActive("retention-")
        || ConfigurationPageOperationActive("update-settings-")
        || _updateSettingsBusyScopeCount > 0
        || _updateBusyScopeCount > 0;

    private bool ConfigurationPageOperationActive(string operationPrefix) =>
        _activeOperations.Any(operation =>
            operation.StartsWith(operationPrefix, StringComparison.Ordinal));

    private ConfigurationPageEditState CurrentConfigurationPageEdits() => new(
        SourcesHaveUnsavedChanges(),
        RepositoryHasUnsavedChanges(),
        ScheduleHasUnsavedChanges(),
        RetentionHasUnsavedChanges());

    private void ShowConfigurationRefreshDeferred()
    {
        if (_configurationRefreshDeferredForEdits)
        {
            return;
        }

        _configurationRefreshDeferredForEdits = true;
        DiscardConfigurationEditsButton.IsEnabled = true;
        ManagedRefreshInfoBar.Message = ConfigurationRefreshDeferredMessage;
        ManagedRefreshInfoBar.IsOpen = true;
    }

    private void ClearConfigurationRefreshDeferred()
    {
        _configurationRefreshDeferredForEdits = false;
        DiscardConfigurationEditsButton.IsEnabled = true;
        ManagedRefreshInfoBar.IsOpen = false;
    }

    private void RefreshConfigurationControlStates()
    {
        NavigationRoot.IsEnabled = !_configurationEditGate.ReloadInProgress;
        RefreshSourcesControlState();
        SetRepositoryBusy(false);
        SetScheduleBusy(false);
        SetRetentionBusy(false);
        RefreshUpdateControlState();
    }

    private async void DiscardConfigurationEditsButton_Click(object sender, RoutedEventArgs e)
    {
        DiscardConfigurationEditsButton.IsEnabled = false;
        _discardConfigurationEditsRequested = true;
        IDisposable reloadScope = _configurationEditGate.BeginReload();
        RefreshConfigurationControlStates();
        try
        {
            await RefreshStatusAsync(synchronizeConfiguration: true);
        }
        finally
        {
            _discardConfigurationEditsRequested = false;
            reloadScope.Dispose();
            RefreshConfigurationControlStates();
            DiscardConfigurationEditsButton.IsEnabled = true;
        }
    }

    private bool TryReadDurationSeconds(
        NumberBox box,
        string fieldName,
        bool allowZero,
        out ulong seconds)
    {
        double minimum = allowZero ? 0 : 1.0 / 60.0;
        if (double.IsNaN(box.Value) || box.Value < minimum || box.Value > 1_440)
        {
            seconds = 0;
            ShowMessage(
                InfoBarSeverity.Warning,
                $"{fieldName} must be between {(allowZero ? "zero" : "one second")} and 24 hours.");
            return false;
        }

        seconds = checked((ulong)Math.Round(box.Value * 60, MidpointRounding.AwayFromZero));
        return true;
    }

    private bool TryReadWholeNumber(
        NumberBox box,
        string fieldName,
        ulong minimum,
        ulong maximum,
        out ulong value)
    {
        if (double.IsNaN(box.Value)
            || box.Value < minimum
            || box.Value > maximum
            || box.Value != Math.Truncate(box.Value))
        {
            value = 0;
            ShowMessage(
                InfoBarSeverity.Warning,
                $"{fieldName} must be a whole number from {minimum:N0} through {maximum:N0}.");
            return false;
        }

        value = checked((ulong)box.Value);
        return true;
    }

    private async Task<ServiceSnapshot?> RefreshStatusAsync(
        bool synchronizeConfiguration = false)
    {
        try
        {
            ServiceSnapshot status = await _service.GetStatusAsync();
            _lastServiceSnapshot = status;
            bool attemptChanged = status.LastAttempt is not null
                && status.LastAttempt != _manualBackupBaselineAttempt;
            ManualBackupStatusDecision transition = ManualBackupStatusTransition.Evaluate(
                _manualBackupPending,
                ManualBackupRequestElapsed(),
                attemptChanged,
                status.State);
            if (transition.ClearPending)
            {
                ClearManualBackupPending();
            }
            if (transition.ShowRequested)
            {
                ViewWarningDetailsButton.Visibility = Visibility.Collapsed;
                ApplyStatus(
                    "Backup requested",
                    "The service accepted the request and is preparing to start the backup.",
                    canRunBackup: false,
                    canCancelBackup: false);
            }
            else
            {
                ViewWarningDetailsButton.Visibility = status.State == "succeeded_with_warnings"
                    ? Visibility.Visible
                    : Visibility.Collapsed;
                ApplyStatus(
                    status.Headline,
                    status.Description,
                    status.CanRunBackup,
                    status.CanCancelBackup);
            }
            UpdateStatusRefreshCadence();
            bool configurationSynchronized =
                await _managedConfigurationSynchronization.ObserveAsync(
                    status.ManagedRevision,
                    synchronizeConfiguration,
                    SynchronizeConfigurationPagesCoreAsync);
            if (configurationSynchronized && !_managedConfigurationSynchronization.Pending)
            {
                ClearConfigurationRefreshDeferred();
            }
            if (UpdateInstallationDecision.ShouldRecoverSettings(
                    _updateSettingsLoadAttempted,
                    _updateSettingsLoaded,
                    _updateSettingsBusyScopeCount > 0,
                    _updateBusyScopeCount > 0,
                    DateTimeOffset.UtcNow >= _nextUpdateSettingsRecoveryAttempt))
            {
                await LoadUpdateSettingsAsync();
            }
            return status;
        }
        catch (Exception exception)
        {
            StatusTitle.Text = "Service unavailable";
            StatusDescription.Text = "The resticpal service could not be reached.";
            StatusCardTitle.Text = "Not connected";
            StatusCardDescription.Text = "Start or repair the resticpal service, then reopen this window.";
            RunBackupButton.IsEnabled = false;
            CancelBackupButton.IsEnabled = false;
            ViewWarningDetailsButton.Visibility = Visibility.Collapsed;
            ShowConnectionError(exception);
            return null;
        }
    }

    private async void StatusRefreshTimer_Tick(object? sender, object e)
    {
        if (_statusRefreshInProgress)
        {
            return;
        }

        _statusRefreshInProgress = true;
        try
        {
            await RefreshStatusAsync();
        }
        finally
        {
            _statusRefreshInProgress = false;
        }
    }

    private void UpdateStatusRefreshCadence()
    {
        _statusRefreshTimer.Interval = _lastServiceSnapshot?.State == "running"
            ? TimeSpan.FromSeconds(1)
            : _manualBackupPending
                && ManualBackupRequestElapsed() < TimeSpan.FromSeconds(15)
                    ? TimeSpan.FromMilliseconds(500)
                    : _manualBackupPending
                        ? TimeSpan.FromSeconds(5)
                        : TimeSpan.FromSeconds(15);
    }

    private TimeSpan ManualBackupRequestElapsed() =>
        _manualBackupRequestedTimestamp is long requestedTimestamp
            ? Stopwatch.GetElapsedTime(requestedTimestamp)
            : TimeSpan.Zero;

    private void ClearManualBackupPending()
    {
        _manualBackupPending = false;
        _manualBackupRequestedTimestamp = null;
        _manualBackupBaselineAttempt = null;
        _historyLoaded = false;
    }

    private void ApplyStatus(
        string headline,
        string description,
        bool canRunBackup,
        bool canCancelBackup)
    {
        StatusTitle.Text = headline;
        StatusDescription.Text = description;
        StatusCardTitle.Text = headline;
        StatusCardDescription.Text = description;
        RunBackupButton.IsEnabled = canRunBackup;
        CancelBackupButton.IsEnabled = canCancelBackup;
    }

    private void ShowConnectionError(Exception exception)
    {
        MessageBar.Severity = InfoBarSeverity.Error;
        MessageBar.Message = exception is OperationCanceledException
            ? "The backup service did not respond in time."
            : string.IsNullOrWhiteSpace(exception.Message)
                ? $"The operation failed unexpectedly (0x{exception.HResult:X8})."
                : exception.Message;
        MessageBar.IsOpen = true;
    }

    private void ShowMessage(InfoBarSeverity severity, string message)
    {
        MessageBar.Severity = severity;
        MessageBar.Message = message;
        MessageBar.IsOpen = true;
    }
}
