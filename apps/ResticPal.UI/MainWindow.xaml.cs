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
    private readonly ResticPalUpdateService _updates = new();
    private readonly bool _showOnboarding;
    private readonly bool _showUpdates;
    private bool _sourcesLoaded;
    private bool _pathsLocked;
    private bool _exclusionsLocked;
    private bool _repositoryLoaded;
    private bool _repositoryDisplayNameLocked;
    private bool _repositoryUrlLocked;
    private bool _repositoryModeLocked;
    private bool _repositoryOptionsLocked;
    private bool _repositorySecretsLocked;
    private bool _repositoryBusy;
    private bool _applyingRepositoryConfiguration;
    private bool _repositoryDirty;
    private bool _scheduleLoaded;
    private bool _scheduleIntervalLocked;
    private bool _wakeGraceLocked;
    private bool _wakeLockTimeoutLocked;
    private bool _allowBatteryLocked;
    private bool _allowMeteredLocked;
    private bool _retentionLoaded;
    private bool _retentionAppendOnly;
    private bool _retentionDailyLocked;
    private bool _retentionWeeklyLocked;
    private bool _retentionMonthlyLocked;
    private bool _retentionYearlyLocked;
    private bool _pruneIntervalLocked;
    private bool _historyLoaded;
    private bool _diagnosticsLoaded;
    private bool _managementLoaded;
    private bool _managedDevice;
    private bool _updateCheckAttempted;
    private AvailableUpdate? _availableUpdate;
    private DownloadedUpdate? _downloadedUpdate;
    private IReadOnlySet<string> _configuredRepositorySecrets = new HashSet<string>();

    public ObservableCollection<string> BackupPaths { get; } = new();
    public ObservableCollection<BackupRunListItem> BackupHistory { get; } = new();
    public ObservableCollection<DiagnosticListItem> Diagnostics { get; } = new();

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

    private async Task LoadManagementAsync()
    {
        SetManagementBusy(true);
        try
        {
            ManagementConfiguration configuration = await _service.GetManagementAsync();
            _managedDevice = configuration.Enrolled;
            ManagementStatusTitle.Text = configuration.Enrolled
                ? "Managed by your backup service"
                : configuration.Mode == "plain_manifest"
                    ? "Using a plain policy file"
                    : "Not enrolled";
            ManagementStatusDescription.Text = configuration.Enrolled
                ? $"Device {configuration.DeviceId} receives signed policy from {configuration.ManifestUrl}."
                : configuration.Mode == "plain_manifest"
                    ? $"Policy is fetched from {configuration.ManifestUrl}; status reporting is disabled."
                    : "Paste a current one-time bootstrap URL to enroll this PC.";
            _managementLoaded = true;
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetManagementBusy(false);
        }
    }

    private async void EnrollButton_Click(object sender, RoutedEventArgs e)
    {
        string bootstrapUrl = BootstrapUrlBox.Password.Trim();
        if (string.IsNullOrWhiteSpace(bootstrapUrl))
        {
            ShowMessage(InfoBarSeverity.Warning, "Paste the one-time bootstrap URL first.");
            return;
        }
        SetManagementBusy(true);
        try
        {
            CommandResult result = await _service.EnrollAsync(bootstrapUrl);
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            if (result.Accepted)
            {
                FirstRunInfoBar.IsOpen = false;
                _managementLoaded = false;
                await LoadManagementAsync();
                await RefreshStatusAsync();
            }
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            BootstrapUrlBox.Password = string.Empty;
            SetManagementBusy(false);
        }
    }

    private async void UnenrollButton_Click(object sender, RoutedEventArgs e)
    {
        var confirmation = new ContentDialog
        {
            Title = "Remove managed backup?",
            Content = "This removes signed policy and status reporting from this PC. Existing backups and repository credentials remain available locally.",
            PrimaryButtonText = "Remove management",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Close,
            XamlRoot = (Content as FrameworkElement)?.XamlRoot,
        };
        if (await confirmation.ShowAsync() != ContentDialogResult.Primary)
        {
            return;
        }
        SetManagementBusy(true);
        try
        {
            CommandResult result = await _service.UnenrollAsync();
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            if (result.Accepted)
            {
                _managementLoaded = false;
                await LoadManagementAsync();
                await RefreshStatusAsync();
            }
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetManagementBusy(false);
        }
    }

    private void SetManagementBusy(bool busy)
    {
        ManagementProgress.IsActive = busy;
        BootstrapUrlBox.IsEnabled = !busy;
        EnrollButton.IsEnabled = !busy;
        UnenrollButton.IsEnabled = !busy && _managedDevice;
    }

    private void ConfigureLocallyButton_Click(object sender, RoutedEventArgs e)
    {
        FirstRunInfoBar.IsOpen = false;
        NavigationRoot.SelectedItem = SourcesItem;
    }

    private void ShowOnboarding()
    {
        FirstRunInfoBar.IsOpen = true;
        NavigationRoot.SelectedItem = NavigationRoot.SettingsItem;
        MarkOnboardingShown();
    }

    private void ShowUpdates()
    {
        NavigationRoot.SelectedItem = NavigationRoot.SettingsItem;
    }

    private void ScrollUpdatesIntoView()
    {
        ManagementPanel.UpdateLayout();
        ManagementPanel.ChangeView(
            horizontalOffset: null,
            verticalOffset: ManagementPanel.ScrollableHeight,
            zoomFactor: null,
            disableAnimation: true);
    }

    private static void MarkOnboardingShown()
    {
        try
        {
            string directory = Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                "resticpal");
            Directory.CreateDirectory(directory);
            File.WriteAllText(Path.Combine(directory, "onboarding-shown-v1"), "1");
        }
        catch
        {
            // A missing marker only means setup may be offered again next login.
        }
    }

    private async Task CheckForUpdatesAsync(bool userInitiated)
    {
        if (_updateCheckAttempted && !userInitiated)
        {
            return;
        }

        _updateCheckAttempted = true;
        SetUpdateBusy(true);
        UpdateStatusDescription.Text = "Checking the signed resticpal release feed…";
        try
        {
            UpdateCheckResult result = await _updates.CheckAsync();
            _availableUpdate = result.Update;
            _downloadedUpdate = null;
            UpdateDownloadProgress.Visibility = Visibility.Collapsed;
            InstallUpdateButton.Visibility = Visibility.Collapsed;

            switch (result.Status)
            {
                case UpdateCheckStatus.Available when result.Update is not null:
                    UpdateStatusDescription.Text =
                        $"resticpal {result.Update.Version} is available. The appcast and installer signatures will both be verified before installation.";
                    DownloadUpdateButton.Visibility = Visibility.Visible;
                    break;
                case UpdateCheckStatus.Current:
                    UpdateStatusDescription.Text =
                        $"resticpal {ResticPalUpdateService.InstalledVersion} is up to date.";
                    DownloadUpdateButton.Visibility = Visibility.Collapsed;
                    if (userInitiated)
                    {
                        ShowMessage(InfoBarSeverity.Success, "You already have the latest resticpal release.");
                    }
                    break;
                default:
                    UpdateStatusDescription.Text =
                        "The signed update feed could not be checked. Your current installation was not changed.";
                    DownloadUpdateButton.Visibility = Visibility.Collapsed;
                    if (userInitiated)
                    {
                        ShowMessage(InfoBarSeverity.Warning, "The signed update feed is unavailable right now.");
                    }
                    break;
            }
        }
        catch (Exception exception)
        {
            _availableUpdate = null;
            _downloadedUpdate = null;
            DownloadUpdateButton.Visibility = Visibility.Collapsed;
            InstallUpdateButton.Visibility = Visibility.Collapsed;
            UpdateStatusDescription.Text =
                "The signed update feed could not be checked. Your current installation was not changed.";
            if (userInitiated)
            {
                ShowMessage(
                    InfoBarSeverity.Warning,
                    exception is OperationCanceledException
                        ? "The update check was cancelled."
                        : "The signed update feed is unavailable right now.");
            }
        }
        finally
        {
            SetUpdateBusy(false);
        }
    }

    private async void CheckForUpdatesButton_Click(object sender, RoutedEventArgs e)
    {
        await CheckForUpdatesAsync(userInitiated: true);
    }

    private async void DownloadUpdateButton_Click(object sender, RoutedEventArgs e)
    {
        if (_availableUpdate is null)
        {
            return;
        }

        var confirmation = new ContentDialog
        {
            Title = $"Download resticpal {_availableUpdate.Version}?",
            Content = "The installer is Authenticode-signed, and NetSparkle will separately verify its pinned Ed25519 signature after download.",
            PrimaryButtonText = "Download",
            CloseButtonText = "Not now",
            DefaultButton = ContentDialogButton.Primary,
            XamlRoot = (Content as FrameworkElement)?.XamlRoot,
        };
        if (await confirmation.ShowAsync() != ContentDialogResult.Primary)
        {
            return;
        }

        SetUpdateBusy(true);
        UpdateDownloadProgress.Value = 0;
        UpdateDownloadProgress.Visibility = Visibility.Visible;
        UpdateStatusDescription.Text = $"Downloading resticpal {_availableUpdate.Version}…";
        try
        {
            var progress = new Progress<int>(percentage =>
            {
                UpdateDownloadProgress.Value = Math.Clamp(percentage, 0, 100);
                UpdateStatusDescription.Text =
                    $"Downloading resticpal {_availableUpdate.Version}… {percentage}%";
            });
            _downloadedUpdate = await _updates.DownloadAsync(_availableUpdate, progress);
            UpdateDownloadProgress.Value = 100;
            UpdateStatusDescription.Text =
                $"resticpal {_availableUpdate.Version} is downloaded and its Ed25519 signature is valid.";
            DownloadUpdateButton.Visibility = Visibility.Collapsed;
            InstallUpdateButton.Visibility = Visibility.Visible;
        }
        catch (Exception exception)
        {
            _downloadedUpdate = null;
            UpdateDownloadProgress.Visibility = Visibility.Collapsed;
            UpdateStatusDescription.Text =
                "The update download failed or did not pass signature verification.";
            ShowMessage(
                InfoBarSeverity.Error,
                exception is OperationCanceledException
                    ? "The update download was cancelled."
                    : "The update was not downloaded or its signature was invalid.");
        }
        finally
        {
            SetUpdateBusy(false);
        }
    }

    private async void InstallUpdateButton_Click(object sender, RoutedEventArgs e)
    {
        if (_downloadedUpdate is null)
        {
            return;
        }

        var confirmation = new ContentDialog
        {
            Title = $"Install resticpal {_downloadedUpdate.Update.Version}?",
            Content = "resticpal will first confirm that no backup is running, briefly hold new backup work, close this window, and start the elevated MSI upgrade.",
            PrimaryButtonText = "Install update",
            CloseButtonText = "Not now",
            DefaultButton = ContentDialogButton.Primary,
            XamlRoot = (Content as FrameworkElement)?.XamlRoot,
        };
        if (await confirmation.ShowAsync() != ContentDialogResult.Primary)
        {
            return;
        }

        SetUpdateBusy(true);
        try
        {
            CommandResult preparation = await _service.PrepareForUpdateAsync();
            if (!preparation.Accepted)
            {
                UpdateStatusDescription.Text = preparation.Message;
                ShowMessage(InfoBarSeverity.Warning, preparation.Message);
                return;
            }

            UpdateStatusDescription.Text = "Starting the signed Windows installer…";
            await _updates.InstallAsync(
                _downloadedUpdate,
                () => Application.Current.Exit());
        }
        catch (Exception exception)
        {
            UpdateStatusDescription.Text = "The Windows installer could not be started.";
            ShowMessage(
                InfoBarSeverity.Error,
                exception is OperationCanceledException
                    ? "The update installation was cancelled."
                    : exception.Message);
        }
        finally
        {
            SetUpdateBusy(false);
        }
    }

    private void SetUpdateBusy(bool busy)
    {
        UpdateProgress.IsActive = busy;
        CheckForUpdatesButton.IsEnabled = !busy;
        DownloadUpdateButton.IsEnabled = !busy && _availableUpdate is not null;
        InstallUpdateButton.IsEnabled = !busy && _downloadedUpdate is not null;
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

    private async void SaveRepositoryButton_Click(object sender, RoutedEventArgs e)
    {
        if (!TryReadRepositoryOptions(out IReadOnlyDictionary<string, string> options, out string error))
        {
            ShowMessage(InfoBarSeverity.Warning, error);
            return;
        }

        SetRepositoryBusy(true);
        try
        {
            var secretUpdates = new List<RepositorySecretUpdate>();
            if (!_repositorySecretsLocked)
            {
                if (RemoveStoredCredentialsCheckBox.IsChecked == true)
                {
                    secretUpdates.AddRange(
                        _configuredRepositorySecrets.Select(RepositorySecretUpdate.Remove));
                }
                else
                {
                    AddSecretUpdate(secretUpdates, "RESTIC_PASSWORD", ResticPasswordBox.Password);
                    AddSecretUpdate(secretUpdates, "AWS_ACCESS_KEY_ID", AwsAccessKeyBox.Password);
                    AddSecretUpdate(secretUpdates, "AWS_SECRET_ACCESS_KEY", AwsSecretKeyBox.Password);
                    AddSecretUpdate(secretUpdates, "AWS_SESSION_TOKEN", AwsSessionTokenBox.Password);
                    AddSecretUpdate(secretUpdates, "AZURE_ACCOUNT_KEY", AzureAccountKeyBox.Password);
                    AddSecretUpdate(secretUpdates, "B2_ACCOUNT_KEY", B2AccountKeyBox.Password);
                    AddSecretUpdate(
                        secretUpdates,
                        "GOOGLE_APPLICATION_CREDENTIALS",
                        GoogleCredentialsBox.Text);
                    AddSecretUpdate(secretUpdates, "RCLONE_CONFIG_PASS", RcloneConfigPasswordBox.Password);
                }
            }

            string? mode = _repositoryModeLocked
                ? null
                : (RepositoryModeBox.SelectedItem as ComboBoxItem)?.Tag as string ?? "standard";
            CommandResult result = await _service.UpdateRepositoryAsync(
                _repositoryDisplayNameLocked ? null : RepositoryDisplayNameBox.Text,
                _repositoryUrlLocked ? null : RepositoryUrlBox.Text,
                mode,
                _repositoryOptionsLocked ? null : options,
                secretUpdates);
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            if (result.Accepted)
            {
                _repositoryLoaded = false;
                await LoadRepositoryAsync();
                await RefreshStatusAsync();
            }
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetRepositoryBusy(false);
        }
    }

    private void RepositoryKindBox_SelectionChanged(
        object sender,
        SelectionChangedEventArgs e)
    {
        if (RepositoryUrlBox is null)
        {
            return;
        }

        RepositoryUrlBox.PlaceholderText = RepositoryKindBox.SelectedIndex switch
        {
            0 => @"C:\Backups\restic or \\server\share\restic",
            1 => "s3:https://storage.example.com/bucket/computer-name",
            _ => "rest:https://backup.example.com/computer-name",
        };
    }

    private void RepositoryModeBox_SelectionChanged(
        object sender,
        SelectionChangedEventArgs e)
    {
        MarkRepositoryDirty();
        if (CreateRepositoryButton is not null)
        {
            CreateRepositoryButton.IsEnabled =
                !_repositoryBusy && !_repositoryDirty && SelectedRepositoryMode() == "standard";
        }
    }

    private void RepositoryField_Changed(object sender, RoutedEventArgs e)
    {
        MarkRepositoryDirty();
    }

    private void MarkRepositoryDirty()
    {
        if (_applyingRepositoryConfiguration)
        {
            return;
        }

        _repositoryDirty = true;
        if (!_repositoryBusy && RepositoryOperationMessage is not null)
        {
            RepositoryOperationMessage.Title = "Save changes first";
            RepositoryOperationMessage.Message =
                "Connection tests and repository creation use the service's saved settings.";
            RepositoryOperationMessage.Severity = InfoBarSeverity.Warning;
            RepositoryOperationMessage.IsOpen = true;
            if (ValidateRepositoryButton is not null)
            {
                ValidateRepositoryButton.IsEnabled = false;
            }
            if (CreateRepositoryButton is not null)
            {
                CreateRepositoryButton.IsEnabled = false;
            }
        }
    }

    private async void ValidateRepositoryButton_Click(object sender, RoutedEventArgs e)
    {
        await RunRepositoryOperationAsync(initialize: false);
    }

    private async void CreateRepositoryButton_Click(object sender, RoutedEventArgs e)
    {
        if (Content is not FrameworkElement root)
        {
            return;
        }
        var confirmation = new ContentDialog
        {
            XamlRoot = root.XamlRoot,
            Title = "Create a new repository?",
            Content = "resticpal will initialize the saved repository location. restic refuses to overwrite an existing repository.",
            PrimaryButtonText = "Create repository",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Close,
        };
        if (await confirmation.ShowAsync() == ContentDialogResult.Primary)
        {
            await RunRepositoryOperationAsync(initialize: true);
        }
    }

    private async Task RunRepositoryOperationAsync(bool initialize)
    {
        SetRepositoryBusy(true);
        try
        {
            CommandResult result = initialize
                ? await _service.InitializeRepositoryAsync()
                : await _service.ValidateRepositoryAsync();
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Informational : InfoBarSeverity.Warning,
                result.Message);
            if (!result.Accepted)
            {
                return;
            }

            RepositoryConfiguration? latest = null;
            for (int attempt = 0; attempt < 260; attempt++)
            {
                await Task.Delay(TimeSpan.FromMilliseconds(500));
                latest = await _service.GetRepositoryAsync();
                ShowRepositoryOperationStatus(latest.OperationStatus);
                if (latest.OperationStatus.State != "running")
                {
                    break;
                }
            }
            if (latest?.OperationStatus.State == "running")
            {
                throw new TimeoutException("The repository operation did not finish within its safety timeout.");
            }

            _repositoryLoaded = false;
            await LoadRepositoryAsync();
            await RefreshStatusAsync();
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetRepositoryBusy(false);
        }
    }

    private async Task LoadRepositoryAsync()
    {
        SetRepositoryBusy(true);
        try
        {
            RepositoryConfiguration configuration = await _service.GetRepositoryAsync();
            _applyingRepositoryConfiguration = true;
            RepositoryDisplayNameBox.Text = configuration.DisplayName ?? string.Empty;
            RepositoryUrlBox.Text = configuration.Url ?? string.Empty;
            RepositoryKindBox.SelectedIndex = RepositoryKindIndex(configuration.Url);
            RepositoryModeBox.SelectedIndex = configuration.Mode == "append_only" ? 1 : 0;
            RepositoryOptionsBox.Text = string.Join(
                Environment.NewLine,
                configuration.Options
                    .OrderBy(option => option.Key, StringComparer.Ordinal)
                    .Select(option => $"{option.Key}={option.Value}"));
            _repositoryDisplayNameLocked = configuration.DisplayNameLocked;
            _repositoryUrlLocked = configuration.UrlLocked;
            _repositoryModeLocked = configuration.ModeLocked;
            _repositoryOptionsLocked = configuration.OptionsLocked;
            _repositorySecretsLocked = configuration.SecretsLocked;
            _configuredRepositorySecrets = configuration.ConfiguredSecrets;
            ClearRepositoryCredentialInputs();
            _applyingRepositoryConfiguration = false;
            _repositoryDirty = false;

            int lockedFields = new[]
            {
                _repositoryDisplayNameLocked,
                _repositoryUrlLocked,
                _repositoryModeLocked,
                _repositoryOptionsLocked,
                _repositorySecretsLocked,
            }.Count(value => value);
            RepositoryPolicyMessage.IsOpen = lockedFields > 0;
            RepositoryPolicyMessage.Message = lockedFields == 5
                ? "Repository settings and credentials are managed by your organization."
                : $"{lockedFields} repository field{(lockedFields == 1 ? " is" : "s are")} managed by your organization.";
            RepositoryCredentialStatus.Text = configuration.ConfiguredSecrets.Count == 0
                ? "No stored credentials. Enter only the values this repository requires."
                : $"Stored securely: {string.Join(", ", configuration.ConfiguredSecrets.Order())}. Leave fields blank to keep them.";
            ShowRepositoryOperationStatus(configuration.OperationStatus);
            _repositoryLoaded = true;
        }
        catch (Exception exception)
        {
            _applyingRepositoryConfiguration = false;
            ShowConnectionError(exception);
        }
        finally
        {
            SetRepositoryBusy(false);
        }
    }

    private void SetRepositoryBusy(bool busy)
    {
        _repositoryBusy = busy;
        RepositoryProgress.IsActive = busy;
        RepositoryDisplayNameBox.IsEnabled = !busy && !_repositoryDisplayNameLocked;
        RepositoryKindBox.IsEnabled = !busy && !_repositoryUrlLocked;
        RepositoryUrlBox.IsEnabled = !busy && !_repositoryUrlLocked;
        RepositoryModeBox.IsEnabled = !busy && !_repositoryModeLocked;
        RepositoryOptionsBox.IsEnabled = !busy && !_repositoryOptionsLocked;
        bool credentialsEnabled = !busy && !_repositorySecretsLocked;
        ResticPasswordBox.IsEnabled = credentialsEnabled;
        AwsAccessKeyBox.IsEnabled = credentialsEnabled;
        AwsSecretKeyBox.IsEnabled = credentialsEnabled;
        AwsSessionTokenBox.IsEnabled = credentialsEnabled;
        AzureAccountKeyBox.IsEnabled = credentialsEnabled;
        B2AccountKeyBox.IsEnabled = credentialsEnabled;
        GoogleCredentialsBox.IsEnabled = credentialsEnabled;
        RcloneConfigPasswordBox.IsEnabled = credentialsEnabled;
        RemoveStoredCredentialsCheckBox.IsEnabled =
            credentialsEnabled && _configuredRepositorySecrets.Count > 0;
        SaveRepositoryButton.IsEnabled = !busy && !(
            _repositoryDisplayNameLocked
            && _repositoryUrlLocked
            && _repositoryModeLocked
            && _repositoryOptionsLocked
            && _repositorySecretsLocked);
        ValidateRepositoryButton.IsEnabled = !busy && !_repositoryDirty;
        CreateRepositoryButton.IsEnabled =
            !busy && !_repositoryDirty && SelectedRepositoryMode() == "standard";
    }

    private string SelectedRepositoryMode() =>
        (RepositoryModeBox.SelectedItem as ComboBoxItem)?.Tag as string ?? "standard";

    private void ShowRepositoryOperationStatus(RepositoryOperationStatus status)
    {
        (RepositoryOperationMessage.Title, RepositoryOperationMessage.Message, RepositoryOperationMessage.Severity) =
            status.State switch
            {
                "validation_required" => (
                    "Connection test required",
                    "Repository connection fields changed. Backups remain paused until the saved settings pass a connection test or repository creation succeeds.",
                    InfoBarSeverity.Warning),
                "running" when status.Operation == "initialize" => (
                    "Creating repository",
                    "The service is initializing the repository. This window can be closed without stopping the operation.",
                    InfoBarSeverity.Informational),
                "running" => (
                    "Testing connection",
                    "The service is verifying the saved repository and credentials.",
                    InfoBarSeverity.Informational),
                "succeeded" => (
                    status.Operation == "initialize" ? "Repository created" : "Connection verified",
                    status.CompletedAt is DateTimeOffset completed
                        ? $"Completed {completed.ToLocalTime():g}. Backups may use this repository."
                        : "Backups may use this repository.",
                    InfoBarSeverity.Success),
                "failed" => (
                    "Repository needs attention",
                    RepositoryFailureMessage(status.Code),
                    InfoBarSeverity.Error),
                _ => (
                    "Connection not tested",
                    "Save the repository settings, then test the connection or create a new repository.",
                    InfoBarSeverity.Informational),
            };
        RepositoryOperationMessage.IsOpen = true;
    }

    private static string RepositoryFailureMessage(string? code) => code switch
    {
        "repository_not_found" =>
            "The location is reachable but does not contain a restic repository. Create one or check the URL.",
        "credential_unavailable" =>
            "A required stored credential is unavailable. Enter it again and save the repository.",
        "repository_operation_timed_out" =>
            "The repository did not respond within two minutes. Check the network and backend address.",
        "repository_initialization_failed" =>
            "restic could not create the repository. Check the address, credentials, and backend permissions.",
        "state_save_failed" =>
            "The result could not be recorded safely. Check the service data directory and try again.",
        _ => "The saved repository or credentials could not be verified. Check them and try again.",
    };

    private bool TryReadRepositoryOptions(
        out IReadOnlyDictionary<string, string> options,
        out string error)
    {
        var parsed = new Dictionary<string, string>(StringComparer.Ordinal);
        foreach (string line in RepositoryOptionsBox.Text.Split(['\r', '\n']))
        {
            if (string.IsNullOrWhiteSpace(line))
            {
                continue;
            }

            int separator = line.IndexOf('=');
            if (separator <= 0)
            {
                options = parsed;
                error = $"Repository option '{line.Trim()}' must use option=value format.";
                return false;
            }

            string key = line[..separator].Trim();
            string value = line[(separator + 1)..].Trim();
            if (!parsed.TryAdd(key, value))
            {
                options = parsed;
                error = $"Repository option '{key}' is listed more than once.";
                return false;
            }
        }

        options = parsed;
        error = string.Empty;
        return true;
    }

    private static void AddSecretUpdate(
        ICollection<RepositorySecretUpdate> updates,
        string variable,
        string value)
    {
        if (!string.IsNullOrEmpty(value))
        {
            updates.Add(RepositorySecretUpdate.Set(variable, value));
        }
    }

    private void ClearRepositoryCredentialInputs()
    {
        ResticPasswordBox.Password = string.Empty;
        AwsAccessKeyBox.Password = string.Empty;
        AwsSecretKeyBox.Password = string.Empty;
        AwsSessionTokenBox.Password = string.Empty;
        AzureAccountKeyBox.Password = string.Empty;
        B2AccountKeyBox.Password = string.Empty;
        GoogleCredentialsBox.Text = string.Empty;
        RcloneConfigPasswordBox.Password = string.Empty;
        RemoveStoredCredentialsCheckBox.IsChecked = false;
    }

    private static int RepositoryKindIndex(string? url)
    {
        if (url?.StartsWith("s3:", StringComparison.OrdinalIgnoreCase) == true)
        {
            return 1;
        }

        if (url is null
            || Path.IsPathRooted(url)
            || url.StartsWith("\\\\", StringComparison.Ordinal))
        {
            return 0;
        }

        return 2;
    }

    private async void SaveScheduleButton_Click(object sender, RoutedEventArgs e)
    {
        if (!TryReadWholeNumber(
                ScheduleIntervalBox,
                "Backup interval",
                1,
                8_760,
                out ulong intervalHours)
            || !TryReadDurationSeconds(
                WakeGraceBox,
                "Wake grace period",
                allowZero: true,
                out ulong wakeGraceSeconds)
            || !TryReadDurationSeconds(
                WakeLockTimeoutBox,
                "Wake-lock timeout",
                allowZero: false,
                out ulong wakeLockTimeoutSeconds))
        {
            return;
        }

        SetScheduleBusy(true);
        try
        {
            CommandResult result = await _service.UpdateScheduleAsync(
                _scheduleIntervalLocked ? null : checked((uint)intervalHours),
                _wakeGraceLocked ? null : wakeGraceSeconds,
                _wakeLockTimeoutLocked ? null : wakeLockTimeoutSeconds,
                _allowBatteryLocked ? null : AllowBatteryToggle.IsOn,
                _allowMeteredLocked ? null : AllowMeteredToggle.IsOn);
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            if (result.Accepted)
            {
                _scheduleLoaded = false;
                await LoadScheduleAsync();
                await RefreshStatusAsync();
            }
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetScheduleBusy(false);
        }
    }

    private async Task LoadScheduleAsync()
    {
        SetScheduleBusy(true);
        try
        {
            ScheduleConfiguration configuration = await _service.GetScheduleAsync();
            ScheduleIntervalBox.Value = configuration.IntervalHours;
            WakeGraceBox.Value = configuration.WakeGraceSeconds / 60.0;
            WakeLockTimeoutBox.Value = configuration.WakeLockTimeoutSeconds / 60.0;
            AllowBatteryToggle.IsOn = configuration.AllowOnBattery;
            AllowMeteredToggle.IsOn = configuration.AllowMeteredNetwork;
            _scheduleIntervalLocked = configuration.IntervalHoursLocked;
            _wakeGraceLocked = configuration.WakeGraceSecondsLocked;
            _wakeLockTimeoutLocked = configuration.WakeLockTimeoutSecondsLocked;
            _allowBatteryLocked = configuration.AllowOnBatteryLocked;
            _allowMeteredLocked = configuration.AllowMeteredNetworkLocked;

            int lockedFields = new[]
            {
                _scheduleIntervalLocked,
                _wakeGraceLocked,
                _wakeLockTimeoutLocked,
                _allowBatteryLocked,
                _allowMeteredLocked,
            }.Count(value => value);
            SchedulePolicyMessage.IsOpen = lockedFields > 0;
            SchedulePolicyMessage.Message = lockedFields == 5
                ? "The backup schedule and power/network behavior are managed by your organization."
                : $"{lockedFields} schedule field{(lockedFields == 1 ? " is" : "s are")} managed by your organization.";
            _scheduleLoaded = true;
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetScheduleBusy(false);
        }
    }

    private void SetScheduleBusy(bool busy)
    {
        ScheduleProgress.IsActive = busy;
        ScheduleIntervalBox.IsEnabled = !busy && !_scheduleIntervalLocked;
        WakeGraceBox.IsEnabled = !busy && !_wakeGraceLocked;
        WakeLockTimeoutBox.IsEnabled = !busy && !_wakeLockTimeoutLocked;
        AllowBatteryToggle.IsEnabled = !busy && !_allowBatteryLocked;
        AllowMeteredToggle.IsEnabled = !busy && !_allowMeteredLocked;
        SaveScheduleButton.IsEnabled = !busy && !(
            _scheduleIntervalLocked
            && _wakeGraceLocked
            && _wakeLockTimeoutLocked
            && _allowBatteryLocked
            && _allowMeteredLocked);
    }

    private async void SaveRetentionButton_Click(object sender, RoutedEventArgs e)
    {
        if (!TryReadWholeNumber(RetentionDailyBox, "Daily retention", 0, 10_000, out ulong daily)
            || !TryReadWholeNumber(RetentionWeeklyBox, "Weekly retention", 0, 10_000, out ulong weekly)
            || !TryReadWholeNumber(RetentionMonthlyBox, "Monthly retention", 0, 10_000, out ulong monthly)
            || !TryReadWholeNumber(RetentionYearlyBox, "Yearly retention", 0, 10_000, out ulong yearly)
            || !TryReadWholeNumber(PruneIntervalBox, "Prune interval", 1, 365, out ulong pruneDays))
        {
            return;
        }
        if (daily == 0 && weekly == 0 && monthly == 0 && yearly == 0)
        {
            ShowMessage(InfoBarSeverity.Warning, "Keep at least one daily, weekly, monthly, or yearly snapshot.");
            return;
        }

        SetRetentionBusy(true);
        try
        {
            CommandResult result = await _service.UpdateRetentionAsync(
                _retentionDailyLocked ? null : checked((uint)daily),
                _retentionWeeklyLocked ? null : checked((uint)weekly),
                _retentionMonthlyLocked ? null : checked((uint)monthly),
                _retentionYearlyLocked ? null : checked((uint)yearly),
                _pruneIntervalLocked ? null : checked((uint)pruneDays));
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            if (result.Accepted)
            {
                _retentionLoaded = false;
                await LoadRetentionAsync();
            }
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetRetentionBusy(false);
        }
    }

    private async Task LoadRetentionAsync()
    {
        SetRetentionBusy(true);
        try
        {
            RetentionConfiguration configuration = await _service.GetRetentionAsync();
            RetentionDailyBox.Value = configuration.Daily;
            RetentionWeeklyBox.Value = configuration.Weekly;
            RetentionMonthlyBox.Value = configuration.Monthly;
            RetentionYearlyBox.Value = configuration.Yearly;
            PruneIntervalBox.Value = configuration.PruneIntervalDays;
            _retentionAppendOnly = configuration.RepositoryMode == "append_only";
            _retentionDailyLocked = configuration.DailyLocked;
            _retentionWeeklyLocked = configuration.WeeklyLocked;
            _retentionMonthlyLocked = configuration.MonthlyLocked;
            _retentionYearlyLocked = configuration.YearlyLocked;
            _pruneIntervalLocked = configuration.PruneIntervalDaysLocked;

            int lockedFields = new[]
            {
                _retentionDailyLocked,
                _retentionWeeklyLocked,
                _retentionMonthlyLocked,
                _retentionYearlyLocked,
                _pruneIntervalLocked,
            }.Count(value => value);
            RetentionPolicyMessage.IsOpen = _retentionAppendOnly || lockedFields > 0;
            RetentionPolicyMessage.Message = _retentionAppendOnly
                ? "Managed by server: this append-only client cannot forget snapshots or prune repository data."
                : lockedFields == 5
                    ? "The retention policy is managed by your organization."
                    : $"{lockedFields} retention field{(lockedFields == 1 ? " is" : "s are")} managed by your organization.";

            var status = new List<string>();
            if (configuration.LastRetention is DateTimeOffset retention)
            {
                status.Add($"Last retention: {retention.ToLocalTime():g}");
            }
            if (configuration.LastPrune is DateTimeOffset prune)
            {
                status.Add($"last prune: {prune.ToLocalTime():g}");
            }
            if (!string.IsNullOrWhiteSpace(configuration.LastError))
            {
                status.Add($"latest warning: {configuration.LastError}");
            }
            RetentionLastRunText.Text = status.Count == 0
                ? (_retentionAppendOnly ? "Maintenance history is held by the repository server." : "Retention has not run yet.")
                : string.Join(" · ", status);
            _retentionLoaded = true;
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetRetentionBusy(false);
        }
    }

    private void SetRetentionBusy(bool busy)
    {
        RetentionProgress.IsActive = busy;
        RetentionDailyBox.IsEnabled = !busy && !_retentionAppendOnly && !_retentionDailyLocked;
        RetentionWeeklyBox.IsEnabled = !busy && !_retentionAppendOnly && !_retentionWeeklyLocked;
        RetentionMonthlyBox.IsEnabled = !busy && !_retentionAppendOnly && !_retentionMonthlyLocked;
        RetentionYearlyBox.IsEnabled = !busy && !_retentionAppendOnly && !_retentionYearlyLocked;
        PruneIntervalBox.IsEnabled = !busy && !_retentionAppendOnly && !_pruneIntervalLocked;
        SaveRetentionButton.IsEnabled = !busy && !_retentionAppendOnly && !(
            _retentionDailyLocked
            && _retentionWeeklyLocked
            && _retentionMonthlyLocked
            && _retentionYearlyLocked
            && _pruneIntervalLocked);
    }

    private async void RefreshHistoryButton_Click(object sender, RoutedEventArgs e)
    {
        await LoadHistoryAsync();
    }

    private async Task LoadHistoryAsync()
    {
        SetHistoryBusy(true);
        try
        {
            IReadOnlyList<BackupRun> runs = await _service.GetRunHistoryAsync();
            BackupHistory.Clear();
            foreach (BackupRun run in runs)
            {
                BackupHistory.Add(new BackupRunListItem(run));
            }

            HistoryEmptyCard.Visibility = BackupHistory.Count == 0
                ? Visibility.Visible
                : Visibility.Collapsed;
            HistoryList.Visibility = BackupHistory.Count == 0
                ? Visibility.Collapsed
                : Visibility.Visible;
            _historyLoaded = true;
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetHistoryBusy(false);
        }
    }

    private void SetHistoryBusy(bool busy)
    {
        HistoryProgress.IsActive = busy;
        RefreshHistoryButton.IsEnabled = !busy;
    }

    private async void RefreshDiagnosticsButton_Click(object sender, RoutedEventArgs e)
    {
        await LoadDiagnosticsAsync();
    }

    private async Task LoadDiagnosticsAsync()
    {
        SetDiagnosticsBusy(true);
        try
        {
            IReadOnlyList<DiagnosticRecord> entries = await _service.GetDiagnosticsAsync();
            Diagnostics.Clear();
            foreach (DiagnosticRecord entry in entries.Reverse())
            {
                Diagnostics.Add(new DiagnosticListItem(entry));
            }
            DiagnosticsEmptyCard.Visibility = Diagnostics.Count == 0
                ? Visibility.Visible
                : Visibility.Collapsed;
            DiagnosticsList.Visibility = Diagnostics.Count == 0
                ? Visibility.Collapsed
                : Visibility.Visible;
            _diagnosticsLoaded = true;
        }
        catch (Exception exception)
        {
            ShowConnectionError(exception);
        }
        finally
        {
            SetDiagnosticsBusy(false);
        }
    }

    private void SetDiagnosticsBusy(bool busy)
    {
        DiagnosticsProgress.IsActive = busy;
        RefreshDiagnosticsButton.IsEnabled = !busy;
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

public sealed class BackupRunListItem
{
    internal BackupRunListItem(BackupRun run)
    {
        Headline = run.Outcome switch
        {
            "succeeded" => "Backup completed",
            "succeeded_with_warnings" => "Backup completed with warnings",
            "cancelled" => "Backup cancelled",
            _ => "Backup failed",
        };

        TimeSpan duration = run.CompletedAt >= run.StartedAt
            ? run.CompletedAt - run.StartedAt
            : TimeSpan.Zero;
        CompletedAtText = $"{run.CompletedAt.ToLocalTime():g} · {FormatDuration(duration)}";

        var summary = new List<string>();
        if (run.FilesProcessed is ulong files)
        {
            summary.Add($"{files:N0} files");
        }
        if (run.BytesProcessed is ulong bytes)
        {
            summary.Add($"{FormatBytes(bytes)} processed");
        }
        if (run.DataAdded is ulong added)
        {
            summary.Add($"{FormatBytes(added)} added");
        }
        Summary = summary.Count == 0
            ? "No aggregate file statistics were reported."
            : string.Join(" · ", summary);

        if (!string.IsNullOrWhiteSpace(run.ErrorCode))
        {
            Detail = $"Sanitized error code: {run.ErrorCode}";
        }
        else if (!string.IsNullOrWhiteSpace(run.SnapshotId))
        {
            Detail = $"Snapshot {run.SnapshotId}";
        }
        else
        {
            Detail = "No additional details.";
        }
    }

    public string Headline { get; }
    public string CompletedAtText { get; }
    public string Summary { get; }
    public string Detail { get; }

    private static string FormatDuration(TimeSpan duration)
    {
        if (duration.TotalHours >= 1)
        {
            return $"{(int)duration.TotalHours}h {duration.Minutes}m";
        }
        if (duration.TotalMinutes >= 1)
        {
            return $"{(int)duration.TotalMinutes}m {duration.Seconds}s";
        }
        return $"{Math.Max(0, (int)duration.TotalSeconds)}s";
    }

    private static string FormatBytes(ulong bytes)
    {
        string[] units = ["B", "KB", "MB", "GB", "TB"];
        double value = bytes;
        int unit = 0;
        while (value >= 1024 && unit < units.Length - 1)
        {
            value /= 1024;
            unit++;
        }
        return $"{value:0.#} {units[unit]}";
    }
}

public sealed class DiagnosticListItem
{
    internal DiagnosticListItem(DiagnosticRecord record)
    {
        Headline = $"{record.Level.Replace('_', ' ')} · {record.EventId}";
        TimestampText = record.Timestamp.ToLocalTime().ToString("g");
        Message = record.Message;
        Detail = string.IsNullOrWhiteSpace(record.Code)
            ? "No error code."
            : $"Sanitized code: {record.Code}";
    }

    public string Headline { get; }
    public string TimestampText { get; }
    public string Message { get; }
    public string Detail { get; }
}
