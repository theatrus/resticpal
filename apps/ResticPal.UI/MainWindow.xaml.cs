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
    private readonly ResticPalServiceClient _service = new();
    private readonly ResticPalUpdateService _updates = new();
    private readonly bool _showOnboarding;
    private readonly bool _showUpdates;
    private readonly HashSet<string> _activeOperations = new(StringComparer.Ordinal);
    private readonly DispatcherTimer _statusRefreshTimer = new();
    private bool _statusRefreshInProgress;
    private bool _manualBackupPending;
    private DateTimeOffset? _manualBackupRequestedAt;
    private DateTimeOffset? _manualBackupBaselineAttempt;
    private ServiceSnapshot? _lastServiceSnapshot;

    public MainWindow(bool showOnboarding = false, bool showUpdates = false)
    {
        _showOnboarding = showOnboarding;
        _showUpdates = showUpdates;
        InitializeComponent();
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
            MessageBar.Severity = result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning;
            MessageBar.Message = result.Message;
            MessageBar.IsOpen = true;
            if (result.Accepted)
            {
                _manualBackupPending = true;
                _manualBackupRequestedAt = DateTimeOffset.UtcNow;
                _manualBackupBaselineAttempt = baselineAttempt;
                ApplyStatus(
                    "Backup requested",
                    "The service accepted the request and is preparing to start the backup.",
                    canRunBackup: false,
                    canCancelBackup: false);
                UpdateStatusRefreshCadence();
                _statusRefreshTimer.Start();
                await Task.Delay(TimeSpan.FromMilliseconds(250));
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
            MessageBar.Severity = result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning;
            MessageBar.Message = result.Message;
            MessageBar.IsOpen = true;
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
            setBusy?.Invoke(false);
            _activeOperations.Remove(operation);
        }
    }

    /// <summary>
    /// Invalidates every view backed by effective configuration and eagerly
    /// reloads the ones the administrator has already visited. Enrollment and
    /// unenrollment can replace several independently locked fields at once;
    /// keeping the old page caches would expose stale values and edit controls.
    /// </summary>
    private async Task SynchronizeConfigurationPagesAsync()
    {
        bool reloadSources = _sourcesLoaded;
        bool reloadRepository = _repositoryLoaded;
        bool reloadSchedule = _scheduleLoaded;
        bool reloadRetention = _retentionLoaded;

        _sourcesLoaded = false;
        _repositoryLoaded = false;
        _scheduleLoaded = false;
        _retentionLoaded = false;

        if (reloadSources)
        {
            await LoadBackupSourcesAsync();
        }
        if (reloadRepository)
        {
            await LoadRepositoryAsync();
        }
        if (reloadSchedule)
        {
            await LoadScheduleAsync();
        }
        if (reloadRetention)
        {
            await LoadRetentionAsync();
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

    private async Task<ServiceSnapshot?> RefreshStatusAsync()
    {
        try
        {
            ServiceSnapshot status = await _service.GetStatusAsync();
            _lastServiceSnapshot = status;
            bool attemptChanged = status.LastAttempt is not null
                && status.LastAttempt != _manualBackupBaselineAttempt;
            bool stalePreRequestStatus = _manualBackupPending
                && !attemptChanged
                && status.State is not ("running" or "waiting");
            if (stalePreRequestStatus)
            {
                ApplyStatus(
                    "Backup requested",
                    "The service accepted the request and is preparing to start the backup.",
                    canRunBackup: false,
                    canCancelBackup: false);
            }
            else
            {
                ApplyStatus(
                    status.Headline,
                    status.Description,
                    status.CanRunBackup,
                    status.CanCancelBackup);
            }

            if (_manualBackupPending
                && attemptChanged
                && status.State is not ("running" or "waiting"))
            {
                _manualBackupPending = false;
                _manualBackupRequestedAt = null;
                _historyLoaded = false;
            }
            UpdateStatusRefreshCadence();
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
                && _manualBackupRequestedAt is DateTimeOffset requestedAt
                && DateTimeOffset.UtcNow - requestedAt < TimeSpan.FromSeconds(15)
                    ? TimeSpan.FromMilliseconds(500)
                    : _manualBackupPending
                        ? TimeSpan.FromSeconds(5)
                        : TimeSpan.FromSeconds(15);
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
