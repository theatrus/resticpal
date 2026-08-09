using System.Collections.ObjectModel;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;
using Windows.Storage;
using Windows.Storage.Pickers;

namespace ResticPal.UI;

public sealed partial class MainWindow : Window
{
    private readonly ResticPalServiceClient _service = new();
    private bool _sourcesLoaded;
    private bool _pathsLocked;
    private bool _exclusionsLocked;

    public ObservableCollection<string> BackupPaths { get; } = new();

    public MainWindow()
    {
        InitializeComponent();
    }

    private async void NavigationView_Loaded(object sender, RoutedEventArgs e)
    {
        await RefreshStatusAsync();
    }

    private async void NavigationView_SelectionChanged(
        NavigationView sender,
        NavigationViewSelectionChangedEventArgs args)
    {
        string tag = (args.SelectedItemContainer?.Tag as string) ?? "settings";
        OverviewPanel.Visibility = tag == "overview" ? Visibility.Visible : Visibility.Collapsed;
        SourcesPanel.Visibility = tag == "sources" ? Visibility.Visible : Visibility.Collapsed;
        ComingSoonPanel.Visibility = tag is not ("overview" or "sources")
            ? Visibility.Visible
            : Visibility.Collapsed;

        if (tag == "sources" && !_sourcesLoaded)
        {
            await LoadBackupSourcesAsync();
        }
        else if (tag is not ("overview" or "sources"))
        {
            ComingSoonTitle.Text = args.IsSettingsSelected
                ? "Application settings"
                : args.SelectedItemContainer?.Content?.ToString() ?? "Coming soon";
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

    private async void DiscoverSourcesButton_Click(object sender, RoutedEventArgs e)
    {
        SetSourcesBusy(true);
        try
        {
            IReadOnlyList<DiscoveredBackupSource> suggestions =
                await _service.DiscoverBackupSourcesAsync();
            int added = 0;
            foreach (DiscoveredBackupSource suggestion in suggestions)
            {
                if (BackupPaths.Any(path =>
                    string.Equals(path, suggestion.Path, StringComparison.OrdinalIgnoreCase)))
                {
                    continue;
                }

                BackupPaths.Add(suggestion.Path);
                added++;
            }

            ShowMessage(
                added > 0 ? InfoBarSeverity.Success : InfoBarSeverity.Informational,
                added > 0
                    ? $"Added {added} standard user folder{(added == 1 ? string.Empty : "s")}. Review the list, then save."
                    : "All discovered standard user folders are already listed.");
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetSourcesBusy(false);
        }
    }

    private async void AddSourceButton_Click(object sender, RoutedEventArgs e)
    {
        try
        {
            var picker = new FolderPicker();
            picker.FileTypeFilter.Add("*");
            nint windowHandle = WinRT.Interop.WindowNative.GetWindowHandle(this);
            WinRT.Interop.InitializeWithWindow.Initialize(picker, windowHandle);
            StorageFolder? folder = await picker.PickSingleFolderAsync();
            if (folder is null
                || BackupPaths.Any(path =>
                    string.Equals(path, folder.Path, StringComparison.OrdinalIgnoreCase)))
            {
                return;
            }

            BackupPaths.Add(folder.Path);
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
    }

    private void RemoveSourceButton_Click(object sender, RoutedEventArgs e)
    {
        if (BackupPathsList.SelectedItem is string path)
        {
            BackupPaths.Remove(path);
        }
    }

    private async void SaveSourcesButton_Click(object sender, RoutedEventArgs e)
    {
        SetSourcesBusy(true);
        try
        {
            string[] exclusions = ExclusionsBox.Text
                .Split(['\r', '\n'], StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
                .Where(value => !string.IsNullOrWhiteSpace(value))
                .Distinct(StringComparer.Ordinal)
                .ToArray();
            CommandResult result = await _service.UpdateBackupSourcesAsync(
                _pathsLocked ? null : BackupPaths.ToArray(),
                _exclusionsLocked ? null : exclusions);
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            if (result.Accepted)
            {
                _sourcesLoaded = false;
                await LoadBackupSourcesAsync();
                await RefreshStatusAsync();
            }
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetSourcesBusy(false);
        }
    }

    private async Task LoadBackupSourcesAsync()
    {
        SetSourcesBusy(true);
        try
        {
            BackupSourcesConfiguration configuration = await _service.GetBackupSourcesAsync();
            BackupPaths.Clear();
            foreach (string path in configuration.Paths)
            {
                BackupPaths.Add(path);
            }
            ExclusionsBox.Text = string.Join(Environment.NewLine, configuration.Exclusions);
            _pathsLocked = configuration.PathsLocked;
            _exclusionsLocked = configuration.ExclusionsLocked;
            SourcesPolicyMessage.IsOpen = _pathsLocked || _exclusionsLocked;
            SourcesPolicyMessage.Message = (_pathsLocked, _exclusionsLocked) switch
            {
                (true, true) => "Backup paths and exclusions are managed by your organization.",
                (true, false) => "Backup paths are managed by your organization. Local exclusions remain editable.",
                (false, true) => "Exclusions are managed by your organization. Local backup paths remain editable.",
                _ => string.Empty,
            };
            _sourcesLoaded = true;
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetSourcesBusy(false);
        }
    }

    private void SetSourcesBusy(bool busy)
    {
        SourcesProgress.IsActive = busy;
        DiscoverSourcesButton.IsEnabled = !busy && !_pathsLocked;
        AddSourceButton.IsEnabled = !busy && !_pathsLocked;
        RemoveSourceButton.IsEnabled = !busy && !_pathsLocked;
        ExclusionsBox.IsEnabled = !busy && !_exclusionsLocked;
        SaveSourcesButton.IsEnabled = !busy && !(_pathsLocked && _exclusionsLocked);
    }

    private async Task RefreshStatusAsync()
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
