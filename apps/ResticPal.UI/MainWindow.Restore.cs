using System.Collections.ObjectModel;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.Windows.Storage.Pickers;
using ResticPal.UI.Services;
using Windows.Storage;
using Windows.System;

namespace ResticPal.UI;

/// <summary>
/// Administrator-only repository browsing and verified, non-overwriting restore.
/// Repository credentials and restic execution always remain inside the service.
/// </summary>
public sealed partial class MainWindow
{
    private static readonly TimeSpan RestoreQueryTimeout = TimeSpan.FromMinutes(5);
    private readonly List<RestoreSnapshot> _restoreSnapshots = [];
    private readonly List<RestoreSourceRoot> _restoreSourceRoots = [];
    private readonly CancellationTokenSource _restoreWindowCancellation = new();
    private readonly DispatcherTimer _restoreStatusTimer = new()
    {
        Interval = TimeSpan.FromSeconds(1),
    };
    private CancellationTokenSource? _restoreQueryCancellation;
    private RestoreSettingsConfiguration? _restoreSettings;
    private RestoreOperationStatus? _lastRestoreStatus;
    private string? _restoreRequestedSnapshotId;
    private string? _restoreDestination;
    private ulong? _activeRestoreQueryId;
    private ulong? _activeRestoreJobId;
    private int _restoreBrowseGeneration;
    private bool _restoreLoaded;
    private bool _restoreSettingsLoaded;
    private bool _restoreApplyingSettings;
    private bool _restoreUpdatingSelectors;
    private bool _restorePageBusy;
    private bool _restoreStatusRefreshInProgress;
    private bool _restoreCancellationRequested;

    public ObservableCollection<RestoreSnapshotListItem> RestoreSnapshotOptions { get; } = [];

    public ObservableCollection<RestoreBrowserEntryListItem> RestoreEntries { get; } = [];

    public ObservableCollection<RestoreBreadcrumb> RestoreBreadcrumbs { get; } = [];

    private void InitializeRestoreTracking()
    {
        _restoreStatusTimer.Tick += RestoreStatusTimer_Tick;
        SetRestoreBreadcrumbs("/");
    }

    private void StopRestoreTracking()
    {
        _restoreStatusTimer.Stop();
        _restoreStatusTimer.Tick -= RestoreStatusTimer_Tick;
        CancelPendingRestoreQuery();
        _restoreWindowCancellation.Cancel();
    }

    private async Task LoadRestorePageAsync(bool refreshSnapshots)
    {
        await LoadRestoreSettingsAsync();
        if (_restoreSettings is not { Enabled: true })
        {
            await ReconnectActiveRestoreWhileDisabledAsync();
            return;
        }

        bool statusLoaded = false;
        await RunGuardedAsync("restore-status-load", async () =>
        {
            await RefreshRestoreStatusAsync();
            statusLoaded = true;
        });
        if (!statusLoaded)
        {
            return;
        }
        if ((refreshSnapshots || !_restoreLoaded) && _activeRestoreJobId is null)
        {
            await LoadRestoreSnapshotsAsync();
        }
    }

    private Task LoadRestoreSettingsAsync() =>
        RunGuardedAsync("restore-settings-load", async () =>
        {
            RestoreSettingsConfiguration configuration =
                await _service.GetRestoreSettingsAsync(_restoreWindowCancellation.Token);
            ApplyRestoreSettings(configuration);
            _restoreSettingsLoaded = true;
        }, SetRestoreSettingsBusy);

    private void ApplyRestoreSettings(RestoreSettingsConfiguration configuration)
    {
        bool previouslyEnabled = _restoreSettings?.Enabled == true;
        _restoreSettings = configuration;
        _restoreApplyingSettings = true;
        try
        {
            RestoreEnabledToggle.IsOn = configuration.Enabled;
        }
        finally
        {
            _restoreApplyingSettings = false;
        }

        if (configuration.Managed && configuration.EnabledLocked)
        {
            RestoreSettingsDescription.Text = configuration.Enabled
                ? "Your organization allows administrators to restore files on this PC."
                : "Your organization does not permit file restores on this PC.";
            RestorePolicyMessage.Title = configuration.Enabled
                ? "Restore access is managed"
                : "Restore access is disabled by your organization";
            RestorePolicyMessage.Message = configuration.Enabled
                ? "Your organization controls whether administrators may browse or restore backups."
                : "An administrator cannot browse backup snapshots or restore files unless managed policy explicitly allows it.";
            RestorePolicyMessage.Severity = configuration.Enabled
                ? InfoBarSeverity.Informational
                : InfoBarSeverity.Warning;
            RestorePolicyMessage.IsOpen = true;
        }
        else
        {
            RestoreSettingsDescription.Text = configuration.Managed
                ? "Your organization recommends this setting; an administrator may change it locally."
                : "Only an administrator can browse backup snapshots or restore files.";
            RestorePolicyMessage.Title = "Restore access is turned off";
            RestorePolicyMessage.Message =
                "Turn on file restores above to browse backups and restore a verified copy.";
            RestorePolicyMessage.Severity = InfoBarSeverity.Informational;
            RestorePolicyMessage.IsOpen = !configuration.Enabled;
        }

        if (!configuration.Enabled)
        {
            ClearRestoreBrowser();
        }
        else
        {
            RestoreSnapshotCard.Visibility = Visibility.Visible;
            RestoreBrowserCard.Visibility = Visibility.Visible;
            RestoreSelectionCard.Visibility = Visibility.Visible;
            if (!previouslyEnabled)
            {
                _restoreLoaded = false;
                RestoreSnapshotSummary.Text = "Refresh to load backups from the repository.";
            }
        }

        RefreshRestoreControlState();
    }

    private void ClearRestoreBrowser()
    {
        CancelPendingRestoreQuery();
        _restoreBrowseGeneration++;
        _restoreLoaded = false;
        _restoreSnapshots.Clear();
        _restoreSourceRoots.Clear();
        RestoreSnapshotOptions.Clear();
        RestoreEntries.Clear();
        SetRestoreBreadcrumbs("/");
        _restoreUpdatingSelectors = true;
        try
        {
            RestoreDatePicker.Date = null;
            RestoreSnapshotPicker.SelectedItem = null;
        }
        finally
        {
            _restoreUpdatingSelectors = false;
        }

        _restoreDestination = null;
        RestoreSnapshotCard.Visibility = Visibility.Collapsed;
        RestoreBrowserCard.Visibility = Visibility.Collapsed;
        RestoreSelectionCard.Visibility = Visibility.Collapsed;
        RestoreDirectoryMessage.Text = "Restore access is unavailable.";
        RestoreSelectionDescription.Text = "Select one file or folder from the backup above.";
        RestoreDestinationDescription.Text =
            "ResticPal creates a new folder and never replaces existing files.";
        if (_activeRestoreJobId is null)
        {
            _lastRestoreStatus = null;
            _restoreStatusTimer.Stop();
            RestoreProgressCard.Visibility = Visibility.Collapsed;
            RestoreProgressDestination.Text = string.Empty;
            OpenRestoredFolderButton.Visibility = Visibility.Collapsed;
        }
    }

    private async void RestoreEnabledToggle_Toggled(object sender, RoutedEventArgs e)
    {
        if (_restoreApplyingSettings
            || _restoreSettings is not { EnabledLocked: false } settings
            || RestoreEnabledToggle.IsOn == settings.Enabled)
        {
            return;
        }

        bool requested = RestoreEnabledToggle.IsOn;
        await RunGuardedAsync("restore-settings-save", async () =>
        {
            CommandResult result = await _service.UpdateRestoreSettingsAsync(
                requested,
                _restoreWindowCancellation.Token);
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            await LoadRestoreSettingsAsync();
            if (result.Accepted && _restoreSettings?.Enabled == true)
            {
                await LoadRestoreSnapshotsAsync();
            }
        }, SetRestoreSettingsBusy);
    }

    private async void RefreshRestoreButton_Click(object sender, RoutedEventArgs e)
    {
        await LoadRestorePageAsync(refreshSnapshots: true);
    }

    private Task LoadRestoreSnapshotsAsync() =>
        RunGuardedAsync("restore-snapshots-query", async () =>
        {
            if (_restoreSettings?.Enabled != true)
            {
                return;
            }

            RestoreSnapshotSummary.Text = "Loading backups from the repository…";
            int generation = ++_restoreBrowseGeneration;
            using CancellationTokenSource cancellation = BeginRestoreQueryScope();
            try
            {
                ulong queryId = await _service.BeginRestoreSnapshotQueryAsync(cancellation.Token);
                _activeRestoreQueryId = queryId;
                IReadOnlyList<RestoreSnapshot> snapshots = await CompleteRestoreQueryAsync(
                    queryId,
                    "snapshots",
                    result => result.Snapshots,
                    cancellation.Token);
                if (generation != _restoreBrowseGeneration || _restoreSettings?.Enabled != true)
                {
                    return;
                }

                string? preferredId = _restoreRequestedSnapshotId
                    ?? (RestoreSnapshotPicker.SelectedItem as RestoreSnapshotListItem)?.Snapshot.Id;
                _restoreRequestedSnapshotId = null;
                _restoreSnapshots.Clear();
                _restoreSnapshots.AddRange(snapshots.OrderByDescending(snapshot => snapshot.Time));
                _restoreLoaded = true;
                if (_restoreSnapshots.Count == 0)
                {
                    RestoreSnapshotSummary.Text = "This repository does not contain any backups yet.";
                    RestoreSnapshotOptions.Clear();
                    RestoreEntries.Clear();
                    RestoreDirectoryMessage.Text = "Create a backup before browsing files.";
                    UpdateRestoreSelection();
                    return;
                }

                RestoreSnapshot selected = _restoreSnapshots.FirstOrDefault(snapshot =>
                        string.Equals(snapshot.Id, preferredId, StringComparison.Ordinal))
                    ?? _restoreSnapshots[0];
                if (!string.IsNullOrWhiteSpace(preferredId)
                    && !string.Equals(selected.Id, preferredId, StringComparison.Ordinal))
                {
                    ShowMessage(
                        InfoBarSeverity.Warning,
                        "That backup is no longer available. The newest remaining backup was selected.");
                }

                _restoreUpdatingSelectors = true;
                try
                {
                    RestoreDatePicker.Date = selected.Time.ToLocalTime();
                    RefreshSnapshotOptions(selected.Time, selected.Id);
                }
                finally
                {
                    _restoreUpdatingSelectors = false;
                }

                RestoreSnapshotSummary.Text =
                    $"{_restoreSnapshots.Count:N0} available backup{(_restoreSnapshots.Count == 1 ? string.Empty : "s")} · "
                    + $"newest {_restoreSnapshots[0].Time.ToLocalTime():g}";
                await ShowRestoreSnapshotRootAsync(selected);
            }
            catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
            {
                if (!_restoreWindowCancellation.IsCancellationRequested
                    && generation == _restoreBrowseGeneration)
                {
                    throw new TimeoutException("Loading repository snapshots exceeded the restore browser timeout.");
                }
            }
            finally
            {
                CompleteRestoreQueryScope(cancellation);
            }
        }, SetRestorePageBusy);

    private async void RestoreDatePicker_DateChanged(
        CalendarDatePicker sender,
        CalendarDatePickerDateChangedEventArgs args)
    {
        if (_restoreUpdatingSelectors || args.NewDate is not DateTimeOffset selectedDate)
        {
            return;
        }

        _restoreUpdatingSelectors = true;
        try
        {
            RefreshSnapshotOptions(selectedDate, preferredSnapshotId: null);
        }
        finally
        {
            _restoreUpdatingSelectors = false;
        }

        if (RestoreSnapshotPicker.SelectedItem is RestoreSnapshotListItem selected)
        {
            await ShowRestoreSnapshotRootAsync(selected.Snapshot);
        }
        else
        {
            RestoreEntries.Clear();
            _restoreSourceRoots.Clear();
            SetRestoreBreadcrumbs("/");
            RestoreDirectoryMessage.Text = "No backup exists on the selected date.";
            UpdateRestoreSelection();
        }
    }

    private void RefreshSnapshotOptions(DateTimeOffset selectedDate, string? preferredSnapshotId)
    {
        RestoreSnapshotOptions.Clear();
        foreach (RestoreSnapshot snapshot in _restoreSnapshots.Where(snapshot =>
                     RestorePresentation.MatchesLocalDate(snapshot.Time, selectedDate)))
        {
            RestoreSnapshotOptions.Add(new RestoreSnapshotListItem(snapshot));
        }

        RestoreSnapshotPicker.SelectedItem = RestoreSnapshotOptions.FirstOrDefault(snapshot =>
                string.Equals(snapshot.Snapshot.Id, preferredSnapshotId, StringComparison.Ordinal))
            ?? RestoreSnapshotOptions.FirstOrDefault();
        RestoreSnapshotPicker.IsEnabled = RestoreSnapshotOptions.Count > 0
            && !_restorePageBusy
            && _activeRestoreJobId is null;
    }

    private async void RestoreSnapshotPicker_SelectionChanged(
        object sender,
        SelectionChangedEventArgs e)
    {
        if (_restoreUpdatingSelectors
            || RestoreSnapshotPicker.SelectedItem is not RestoreSnapshotListItem selected)
        {
            return;
        }

        await ShowRestoreSnapshotRootAsync(selected.Snapshot);
    }

    private async Task ShowRestoreSnapshotRootAsync(RestoreSnapshot snapshot)
    {
        _restoreSourceRoots.Clear();
        var seenPaths = new HashSet<string>(StringComparer.Ordinal);
        foreach (string source in snapshot.Paths)
        {
            if (RestorePresentation.TrySnapshotSourceRoot(
                    source,
                    out string snapshotPath,
                    out string displayName)
                && seenPaths.Add(snapshotPath))
            {
                _restoreSourceRoots.Add(new RestoreSourceRoot(source, snapshotPath, displayName));
            }
        }

        if (_restoreSourceRoots.Count == 0)
        {
            await LoadRestoreDirectoryAsync(snapshot.Id, "/");
            return;
        }

        CancelPendingRestoreQuery();
        _restoreBrowseGeneration++;
        RestoreEntries.Clear();
        SetRestoreBreadcrumbs("/");
        foreach (RestoreSourceRoot root in _restoreSourceRoots)
        {
            RestoreEntries.Add(new RestoreBrowserEntryListItem(
                new RestoreDirectoryEntry(
                    root.DisplayName,
                    root.SnapshotPath,
                    "directory",
                    Size: null,
                    ModifiedAt: null),
                sourceRoot: true));
        }

        RestoreDirectoryMessage.Text =
            $"{_restoreSourceRoots.Count:N0} backup source{(_restoreSourceRoots.Count == 1 ? string.Empty : "s")} · "
            + "Select a source folder to restore it, or choose Open to browse its files.";
        UpdateRestoreSelection();
    }

    private Task LoadRestoreDirectoryAsync(string snapshotId, string path) =>
        RunGuardedAsync("restore-directory-query", async () =>
        {
            if (_restoreSettings?.Enabled != true)
            {
                return;
            }

            string normalizedPath = RestorePresentation.NormalizeSnapshotPath(path);
            RestoreDirectoryMessage.Text = "Loading this backup folder…";
            RestoreEntries.Clear();
            UpdateRestoreSelection();
            int generation = ++_restoreBrowseGeneration;
            using CancellationTokenSource cancellation = BeginRestoreQueryScope();
            try
            {
                ulong queryId = await _service.BeginRestoreDirectoryQueryAsync(
                    snapshotId,
                    normalizedPath,
                    cancellation.Token);
                _activeRestoreQueryId = queryId;
                IReadOnlyList<RestoreDirectoryEntry> entries = await CompleteRestoreQueryAsync(
                    queryId,
                    "directory",
                    result => result.Entries,
                    cancellation.Token);
                if (generation != _restoreBrowseGeneration
                    || _restoreSettings?.Enabled != true
                    || RestoreSnapshotPicker.SelectedItem is not RestoreSnapshotListItem selected
                    || !string.Equals(selected.Snapshot.Id, snapshotId, StringComparison.Ordinal))
                {
                    return;
                }

                SetRestoreBreadcrumbs(normalizedPath);
                foreach (RestoreDirectoryEntry entry in entries)
                {
                    RestoreEntries.Add(new RestoreBrowserEntryListItem(entry));
                }

                RestoreDirectoryMessage.Text = entries.Count == 0
                    ? "This folder is empty. Use the breadcrumb above to select its parent folder."
                    : $"{entries.Count:N0} item{(entries.Count == 1 ? string.Empty : "s")} · "
                        + "Select a file or folder, or choose Open to browse a folder.";
                UpdateRestoreSelection();
            }
            catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
            {
                if (!_restoreWindowCancellation.IsCancellationRequested
                    && generation == _restoreBrowseGeneration)
                {
                    throw new TimeoutException("Loading the backup folder exceeded the restore browser timeout.");
                }
            }
            finally
            {
                CompleteRestoreQueryScope(cancellation);
            }
        }, SetRestorePageBusy);

    private async Task<IReadOnlyList<T>> CompleteRestoreQueryAsync<T>(
        ulong queryId,
        string expectedKind,
        Func<RestoreQueryResult, IReadOnlyList<T>> selectPage,
        CancellationToken cancellationToken)
    {
        var pages = new RestorePageAccumulator<T>();
        while (true)
        {
            cancellationToken.ThrowIfCancellationRequested();
            RestoreQueryResult result = await _service.GetRestoreQueryAsync(
                queryId,
                pages.Offset,
                limit: 100,
                cancellationToken);
            if (!string.Equals(result.Kind, expectedKind, StringComparison.Ordinal))
            {
                throw new InvalidDataException("The service returned an unexpected restore query type.");
            }

            if (result.State == "running")
            {
                await Task.Delay(TimeSpan.FromMilliseconds(350), cancellationToken);
                continue;
            }
            if (result.State is "failed" or "cancelled")
            {
                throw new InvalidOperationException(
                    result.Message ?? "The repository could not complete this restore query.");
            }
            if (result.State != "succeeded")
            {
                throw new InvalidDataException("The service returned an unknown restore query state.");
            }
            if (pages.Add(selectPage(result), result.Total))
            {
                return pages.Items;
            }
        }
    }

    private CancellationTokenSource BeginRestoreQueryScope()
    {
        CancelPendingRestoreQuery();
        CancellationTokenSource cancellation = CancellationTokenSource.CreateLinkedTokenSource(
            _restoreWindowCancellation.Token);
        cancellation.CancelAfter(RestoreQueryTimeout);
        _restoreQueryCancellation = cancellation;
        return cancellation;
    }

    private void CompleteRestoreQueryScope(CancellationTokenSource cancellation)
    {
        if (ReferenceEquals(_restoreQueryCancellation, cancellation))
        {
            _restoreQueryCancellation = null;
            _activeRestoreQueryId = null;
        }
    }

    private void CancelPendingRestoreQuery()
    {
        CancellationTokenSource? cancellation = _restoreQueryCancellation;
        if (cancellation is null)
        {
            return;
        }

        ulong? queryId = _activeRestoreQueryId;
        _restoreQueryCancellation = null;
        _activeRestoreQueryId = null;
        cancellation.Cancel();
        if (queryId is ulong id && !_restoreWindowCancellation.IsCancellationRequested)
        {
            _ = CancelRestoreQuerySafelyAsync(id);
        }
    }

    private async Task CancelRestoreQuerySafelyAsync(ulong queryId)
    {
        try
        {
            // Closing the window cancels its operation token immediately. Use
            // a separate, bounded token so the best-effort service cancellation
            // can still release the repository for backups and other restores.
            using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(5));
            await _service.CancelRestoreQueryAsync(queryId, cancellation.Token);
        }
        catch (Exception)
        {
            // Cancellation is best effort; service-side query limits remain authoritative.
        }
    }

    private void SetRestoreBreadcrumbs(string path)
    {
        RestoreBreadcrumbs.Clear();
        RestoreSourceRoot? source = _restoreSourceRoots
            .Where(root =>
                string.Equals(path, root.SnapshotPath, StringComparison.Ordinal)
                || path.StartsWith($"{root.SnapshotPath}/", StringComparison.Ordinal))
            .OrderByDescending(root => root.SnapshotPath.Length)
            .FirstOrDefault();
        IReadOnlyList<RestoreBreadcrumb> breadcrumbs = source is null
            ? RestorePresentation.Breadcrumbs(path)
            : RestorePresentation.SourceBreadcrumbs(
                path,
                source.SnapshotPath,
                source.DisplayName);
        foreach (RestoreBreadcrumb breadcrumb in breadcrumbs)
        {
            RestoreBreadcrumbs.Add(breadcrumb);
        }
    }

    private async void RestoreBreadcrumbBar_ItemClicked(
        BreadcrumbBar sender,
        BreadcrumbBarItemClickedEventArgs args)
    {
        if (args.Item is not RestoreBreadcrumb breadcrumb
            || RestoreSnapshotPicker.SelectedItem is not RestoreSnapshotListItem snapshot)
        {
            return;
        }

        if (breadcrumb.Path == "/" && _restoreSourceRoots.Count > 0)
        {
            await ShowRestoreSnapshotRootAsync(snapshot.Snapshot);
        }
        else
        {
            await LoadRestoreDirectoryAsync(snapshot.Snapshot.Id, breadcrumb.Path);
        }
    }

    private async void OpenRestoreDirectoryButton_Click(object sender, RoutedEventArgs e)
    {
        if (sender is not Button button
            || button.Tag is not RestoreBrowserEntryListItem entry
            || !entry.IsDirectory
            || RestoreSnapshotPicker.SelectedItem is not RestoreSnapshotListItem snapshot)
        {
            return;
        }

        await LoadRestoreDirectoryAsync(snapshot.Snapshot.Id, entry.Path);
    }

    private void RestoreEntriesList_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        UpdateRestoreSelection();
    }

    private void UpdateRestoreSelection()
    {
        if (RestoreSelectionDescription is null || StartRestoreButton is null)
        {
            return;
        }

        RestoreSelectionDescription.Text = RestoreEntriesList.SelectedItem
            is RestoreBrowserEntryListItem entry
                ? $"Selected {(entry.IsDirectory ? "folder" : "file")}: {entry.Path}"
                : "Select one file or folder from the backup above.";
        RefreshRestoreControlState();
    }

    private async void ChooseRestoreDestinationButton_Click(object sender, RoutedEventArgs e)
    {
        await RunGuardedAsync("restore-destination", async () =>
        {
            var picker = new FolderPicker(AppWindow.Id)
            {
                CommitButtonText = "Use this folder",
                Title = "Choose where to create the restored backup folder",
            };
            PickFolderResult? destination = await picker.PickSingleFolderAsync();
            if (destination is null)
            {
                return;
            }

            _restoreDestination = destination.Path;
            RestoreDestinationDescription.Text =
                $"A new, separate restore folder will be created inside: {destination.Path}";
            RefreshRestoreControlState();
        }, SetRestorePageBusy);
    }

    private async void StartRestoreButton_Click(object sender, RoutedEventArgs e)
    {
        if (_restoreSettings?.Enabled != true
            || RestoreSnapshotPicker.SelectedItem is not RestoreSnapshotListItem snapshot
            || RestoreEntriesList.SelectedItem is not RestoreBrowserEntryListItem entry
            || string.IsNullOrWhiteSpace(_restoreDestination))
        {
            return;
        }

        string destination = _restoreDestination;
        var confirmation = new ContentDialog
        {
            Title = $"Restore this {(entry.IsDirectory ? "folder" : "file")}?",
            Content = $"Backup: {snapshot.Snapshot.Time.ToLocalTime():f}\n"
                + $"Selected: {entry.Path}\n"
                + $"Destination: {destination}\n\n"
                + "resticpal will create a new local restore folder, verify restored data, "
                + "and never overwrite or delete existing files.",
            PrimaryButtonText = "Restore verified copy",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Primary,
            XamlRoot = NavigationRoot.XamlRoot,
        };
        if (await confirmation.ShowAsync() != ContentDialogResult.Primary)
        {
            return;
        }

        await RunGuardedAsync("restore-start", async () =>
        {
            RestoreSettingsConfiguration latest =
                await _service.GetRestoreSettingsAsync(_restoreWindowCancellation.Token);
            ApplyRestoreSettings(latest);
            if (!latest.Enabled)
            {
                ShowMessage(InfoBarSeverity.Warning, "File restores are no longer allowed on this PC.");
                return;
            }

            _activeRestoreJobId = await _service.StartRestoreAsync(
                snapshot.Snapshot.Id,
                entry.Path,
                destination,
                _restoreWindowCancellation.Token);
            _restoreCancellationRequested = false;
            RestoreProgressCard.Visibility = Visibility.Visible;
            RestoreProgressTitle.Text = "Restore in progress";
            RestoreProgressDescription.Text = "Preparing a verified, non-overwriting restore…";
            RestoreProgressBar.IsIndeterminate = true;
            RestoreProgressDestination.Text = destination;
            CancelRestoreButton.Visibility = Visibility.Visible;
            OpenRestoredFolderButton.Visibility = Visibility.Collapsed;
            _restoreStatusTimer.Start();
            RefreshRestoreControlState();
            await RefreshRestoreStatusAsync();
        }, SetRestorePageBusy);
    }

    private async void CancelRestoreButton_Click(object sender, RoutedEventArgs e)
    {
        if (_activeRestoreJobId is null)
        {
            return;
        }

        await RunGuardedAsync("restore-cancel", async () =>
        {
            CommandResult result = await _service.CancelRestoreAsync(
                _restoreWindowCancellation.Token);
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Informational : InfoBarSeverity.Warning,
                result.Message);
            if (result.Accepted)
            {
                _restoreCancellationRequested = true;
                RestoreProgressTitle.Text = "Cancelling restore";
                RestoreProgressDescription.Text =
                    "Already restored files will remain in the separate destination folder.";
                CancelRestoreButton.IsEnabled = false;
            }

            await RefreshRestoreStatusAsync();
        });
    }

    private async void OpenRestoredFolderButton_Click(object sender, RoutedEventArgs e)
    {
        string? destination = _lastRestoreStatus?.Destination;
        if (string.IsNullOrWhiteSpace(destination))
        {
            return;
        }

        await RunGuardedAsync("restore-open-folder", async () =>
        {
            StorageFolder folder = await StorageFolder.GetFolderFromPathAsync(destination);
            if (!await Launcher.LaunchFolderAsync(folder))
            {
                throw new InvalidOperationException("Windows could not open the restored folder.");
            }
        });
    }

    private async void RestoreStatusTimer_Tick(object? sender, object e)
    {
        if (_restoreStatusRefreshInProgress)
        {
            return;
        }

        _restoreStatusRefreshInProgress = true;
        try
        {
            await RefreshRestoreStatusAsync();
        }
        catch (OperationCanceledException) when (_restoreWindowCancellation.IsCancellationRequested)
        {
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            _restoreStatusRefreshInProgress = false;
        }
    }

    private async Task RefreshRestoreStatusAsync()
    {
        if (_restoreSettings?.Enabled != true && _activeRestoreJobId is null)
        {
            return;
        }

        try
        {
            RestoreOperationStatus status = await _service.GetRestoreStatusAsync(
                _restoreWindowCancellation.Token);
            ApplyRestoreStatus(status);
        }
        catch (RestoreAccessDisabledException) when (_activeRestoreJobId is not null)
        {
            ShowRestoreFinishedAfterPolicyRevocation();
        }
    }

    private Task ReconnectActiveRestoreWhileDisabledAsync() =>
        RunGuardedAsync("restore-status-reconnect", async () =>
        {
            try
            {
                RestoreOperationStatus status = await _service.GetRestoreStatusAsync(
                    _restoreWindowCancellation.Token);
                if (RestorePresentation.IsActiveState(status.State))
                {
                    ApplyRestoreStatus(status);
                }
            }
            catch (RestoreAccessDisabledException)
            {
                // No previously authorized restore remains active under the new policy.
            }
        });

    private void ShowRestoreFinishedAfterPolicyRevocation()
    {
        _activeRestoreJobId = null;
        _lastRestoreStatus = null;
        _restoreCancellationRequested = false;
        _restoreStatusTimer.Stop();
        RestoreProgressCard.Visibility = Visibility.Visible;
        RestoreProgressTitle.Text = "Restore finished after access changed";
        RestoreProgressDescription.Text =
            "The previously authorized restore is no longer running. Recovery access was disabled, "
            + "so its final outcome and folder are no longer available here.";
        RestoreProgressDestination.Text = string.Empty;
        RestoreProgressBar.IsIndeterminate = false;
        RestoreProgressBar.Value = 0;
        CancelRestoreButton.Visibility = Visibility.Collapsed;
        OpenRestoredFolderButton.Visibility = Visibility.Collapsed;
        RefreshRestoreControlState();
    }

    private void ApplyRestoreStatus(RestoreOperationStatus status)
    {
        _lastRestoreStatus = status;
        if (status.State == "idle")
        {
            _activeRestoreJobId = null;
            _restoreCancellationRequested = false;
            _restoreStatusTimer.Stop();
            RestoreProgressCard.Visibility = Visibility.Collapsed;
            RefreshRestoreControlState();
            return;
        }

        bool running = RestorePresentation.IsActiveState(status.State);
        _activeRestoreJobId = running ? status.JobId : null;
        if (!running)
        {
            _restoreCancellationRequested = false;
        }
        RestoreProgressCard.Visibility = Visibility.Visible;
        RestoreProgressTitle.Text = RestorePresentation.OperationTitle(
            status.State,
            _restoreCancellationRequested);
        string? statusMessage = _restoreCancellationRequested
            ? "Already restored files will remain in the separate destination folder."
            : status.Message
                ?? (status.State == "cancelled"
                    ? "Files restored before cancellation remain in the separate destination folder."
                    : null);
        RestoreProgressDescription.Text = RestorePresentation.FormatStatusMessage(
            statusMessage,
            status.FilesRestored,
            status.BytesRestored);
        RestoreProgressDestination.Text = status.Destination is string destination
            ? $"Restore folder: {destination}"
            : string.Empty;

        if (running && status.TotalBytes is > 0)
        {
            RestoreProgressBar.IsIndeterminate = false;
            RestoreProgressBar.Maximum = 100;
            RestoreProgressBar.Value = Math.Min(
                100,
                (double)status.BytesRestored / status.TotalBytes.Value * 100);
        }
        else
        {
            RestoreProgressBar.IsIndeterminate = running;
            RestoreProgressBar.Maximum = 100;
            RestoreProgressBar.Value = status.State is "succeeded" or "completed" ? 100 : 0;
        }

        CancelRestoreButton.Visibility = running ? Visibility.Visible : Visibility.Collapsed;
        CancelRestoreButton.IsEnabled = running && !_restoreCancellationRequested;
        OpenRestoredFolderButton.Visibility = !running
            && !string.IsNullOrWhiteSpace(status.Destination)
                ? Visibility.Visible
                : Visibility.Collapsed;
        if (running)
        {
            _restoreStatusTimer.Start();
        }
        else
        {
            _restoreStatusTimer.Stop();
        }

        RefreshRestoreControlState();
    }

    private void SetRestoreSettingsBusy(bool busy)
    {
        RefreshRestoreControlState(settingsBusy: busy);
    }

    private void SetRestorePageBusy(bool busy)
    {
        _restorePageBusy = busy || ConfigurationPageOperationActive("restore-snapshots-")
            || ConfigurationPageOperationActive("restore-directory-")
            || ConfigurationPageOperationActive("restore-start")
            || ConfigurationPageOperationActive("restore-destination");
        RefreshRestoreControlState();
    }

    private void RefreshRestoreControlState(bool settingsBusy = false)
    {
        if (RestoreEnabledToggle is null)
        {
            return;
        }

        bool activeJob = _activeRestoreJobId is not null;
        bool configurationBusy = settingsBusy
            || ConfigurationPageOperationActive("restore-settings-");
        bool settingsDisabled = _configurationEditGate.ControlsDisabled(
            configurationBusy || _restorePageBusy || activeJob,
            baselineAvailable: _restoreSettingsLoaded);
        bool browsingEnabled = _restoreSettings?.Enabled == true
            && !configurationBusy
            && !_restorePageBusy
            && !activeJob
            && !_configurationEditGate.ReloadInProgress;

        RestoreEnabledToggle.IsEnabled = !settingsDisabled
            && _restoreSettings is { EnabledLocked: false };
        RefreshRestoreButton.IsEnabled = browsingEnabled;
        RestorePageProgress.IsActive = configurationBusy || _restorePageBusy;
        RestoreDatePicker.IsEnabled = browsingEnabled && _restoreSnapshots.Count > 0;
        RestoreSnapshotPicker.IsEnabled = browsingEnabled && RestoreSnapshotOptions.Count > 0;
        RestoreBreadcrumbBar.IsEnabled = browsingEnabled;
        RestoreEntriesList.IsEnabled = browsingEnabled;
        ChooseRestoreDestinationButton.IsEnabled = browsingEnabled;
        StartRestoreButton.IsEnabled = browsingEnabled
            && RestoreEntriesList.SelectedItem is RestoreBrowserEntryListItem
            && RestoreSnapshotPicker.SelectedItem is RestoreSnapshotListItem
            && !string.IsNullOrWhiteSpace(_restoreDestination);
    }

    private sealed record RestoreSourceRoot(
        string WindowsPath,
        string SnapshotPath,
        string DisplayName);
}
