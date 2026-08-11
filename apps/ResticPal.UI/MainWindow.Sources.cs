using System.Collections.ObjectModel;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;
using Windows.Storage;
using Windows.Storage.Pickers;

namespace ResticPal.UI;

/// <summary>Backup sources page: path list, discovery, and exclusions.</summary>
public sealed partial class MainWindow
{
    private bool _sourcesLoaded;
    private bool _pathsLocked;
    private bool _exclusionsLocked;

    public ObservableCollection<string> BackupPaths { get; } = new();

    private async void DiscoverSourcesButton_Click(object sender, RoutedEventArgs e)
    {
        await RunGuardedAsync("sources-discover", async () =>
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
        }, SetSourcesBusy);
    }

    private async void AddSourceButton_Click(object sender, RoutedEventArgs e)
    {
        await RunGuardedAsync("sources-add", async () =>
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
        });
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
        await RunGuardedAsync("sources-save", async () =>
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
        }, SetSourcesBusy);
    }

    private Task LoadBackupSourcesAsync() =>
        RunGuardedAsync("sources-load", async () =>
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
        }, SetSourcesBusy);

    private void SetSourcesBusy(bool busy)
    {
        SourcesProgress.IsActive = busy;
        DiscoverSourcesButton.IsEnabled = !busy && !_pathsLocked;
        AddSourceButton.IsEnabled = !busy && !_pathsLocked;
        RemoveSourceButton.IsEnabled = !busy && !_pathsLocked;
        ExclusionsBox.IsEnabled = !busy && !_exclusionsLocked;
        SaveSourcesButton.IsEnabled = !busy && !(_pathsLocked && _exclusionsLocked);
    }
}
