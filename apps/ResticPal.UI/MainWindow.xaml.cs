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

    public MainWindow(bool showOnboarding = false, bool showUpdates = false)
    {
        _showOnboarding = showOnboarding;
        _showUpdates = showUpdates;
        InitializeComponent();
        AppWindow.SetIcon(Path.Combine(AppContext.BaseDirectory, "resticpal.ico"));
        Closed += (_, _) => _updates.Dispose();
        UpdateStatusDescription.Text =
            $"resticpal {ResticPalUpdateService.InstalledVersion} uses a strictly signed update feed.";
    }

    private async void NavigationView_Loaded(object sender, RoutedEventArgs e)
    {
        ServiceSnapshot? status = await RefreshStatusAsync();
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

        if (tag == "sources" && !_sourcesLoaded)
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
            CommandResult result = await _service.RunBackupNowAsync();
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
            StatusTitle.Text = status.Headline;
            StatusDescription.Text = status.Description;
            StatusCardTitle.Text = status.Headline;
            StatusCardDescription.Text = status.Description;
            RunBackupButton.IsEnabled = status.CanRunBackup;
            CancelBackupButton.IsEnabled = status.CanCancelBackup;
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

    private void ShowConnectionError(Exception exception)
    {
        MessageBar.Severity = InfoBarSeverity.Error;
        MessageBar.Message = exception is OperationCanceledException
            ? "The backup service did not respond in time."
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
