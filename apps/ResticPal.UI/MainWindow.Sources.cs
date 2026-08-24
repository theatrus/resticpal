using System.Collections.ObjectModel;
using Microsoft.Windows.Storage.Pickers;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>Backup sources page: path list, discovery, and exclusions.</summary>
public sealed partial class MainWindow
{
    private bool _sourcesLoaded;
    private bool _pathsLocked;
    private bool _exclusionsLocked;
    private BackupSourcesConfiguration? _loadedBackupSources;

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
            var picker = new FolderPicker(AppWindow.Id)
            {
                CommitButtonText = "Add folder",
                Title = "Choose a folder to back up",
            };
            PickFolderResult? folder = await picker.PickSingleFolderAsync();
            if (folder is null
                || BackupPaths.Any(path =>
                    string.Equals(path, folder.Path, StringComparison.OrdinalIgnoreCase)))
            {
                return;
            }

            BackupPaths.Add(folder.Path);
        }, SetSourcesBusy);
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
        if (_loadedBackupSources is null)
        {
            return;
        }

        bool pathsChanged = SourcesPathsChanged();
        bool exclusionsChanged = SourcesExclusionsChanged();
        if (!pathsChanged && !exclusionsChanged)
        {
            RefreshSourcesControlState();
            return;
        }

        string[] paths = BackupPaths.ToArray();
        string[] exclusions = ReadExclusions();
        await RunGuardedAsync("sources-save", async () =>
        {
            CommandResult result = await _service.UpdateBackupSourcesAsync(
                ConfigurationFieldDiff.ReferenceOrNull(pathsChanged, paths),
                ConfigurationFieldDiff.ReferenceOrNull(exclusionsChanged, exclusions));
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
            _loadedBackupSources = configuration;
            _sourcesLoaded = true;
        }, SetSourcesBusy);

    private string[] ReadExclusions() => ExclusionsBox.Text
        .Split(['\r', '\n'], StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
        .Where(value => !string.IsNullOrWhiteSpace(value))
        .Distinct(StringComparer.Ordinal)
        .ToArray();

    private bool SourcesPathsChanged()
    {
        if (_loadedBackupSources is not BackupSourcesConfiguration loaded)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _pathsLocked,
            string.Join('\0', BackupPaths),
            string.Join('\0', loaded.Paths),
            StringComparer.OrdinalIgnoreCase);
    }

    private bool SourcesExclusionsChanged()
    {
        if (_loadedBackupSources is not BackupSourcesConfiguration loaded)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _exclusionsLocked,
            ExclusionsBox.Text,
            string.Join(Environment.NewLine, loaded.Exclusions),
            StringComparer.Ordinal);
    }

    private bool SourcesHaveUnsavedChanges() =>
        SourcesPathsChanged() || SourcesExclusionsChanged();

    private void SourcesField_Changed(object sender, TextChangedEventArgs e)
    {
        RefreshSourcesControlState();
    }

    private void RefreshSourcesControlState()
    {
        if (SaveSourcesButton is not null)
        {
            SetSourcesBusy(false);
        }
    }

    private void SetSourcesBusy(bool busy)
    {
        bool operationBusy = busy || ConfigurationPageOperationActive("sources-");
        bool controlsDisabled = _configurationEditGate.ControlsDisabled(
            operationBusy,
            baselineAvailable: _loadedBackupSources is not null);
        SourcesProgress.IsActive = operationBusy;
        DiscoverSourcesButton.IsEnabled = !controlsDisabled && !_pathsLocked;
        AddSourceButton.IsEnabled = !controlsDisabled && !_pathsLocked;
        RemoveSourceButton.IsEnabled = !controlsDisabled && !_pathsLocked;
        ExclusionsBox.IsEnabled = !controlsDisabled && !_exclusionsLocked;
        SaveSourcesButton.IsEnabled = !controlsDisabled && SourcesHaveUnsavedChanges();
    }
}
