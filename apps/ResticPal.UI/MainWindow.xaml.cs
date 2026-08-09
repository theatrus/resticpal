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
    private bool _repositoryLoaded;
    private bool _repositoryDisplayNameLocked;
    private bool _repositoryUrlLocked;
    private bool _repositoryModeLocked;
    private bool _repositoryOptionsLocked;
    private bool _repositorySecretsLocked;
    private IReadOnlySet<string> _configuredRepositorySecrets = new HashSet<string>();

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
        RepositoryPanel.Visibility = tag == "repository" ? Visibility.Visible : Visibility.Collapsed;
        ComingSoonPanel.Visibility = tag is not ("overview" or "sources" or "repository")
            ? Visibility.Visible
            : Visibility.Collapsed;

        if (tag == "sources" && !_sourcesLoaded)
        {
            await LoadBackupSourcesAsync();
        }
        else if (tag == "repository" && !_repositoryLoaded)
        {
            await LoadRepositoryAsync();
        }
        else if (tag is not ("overview" or "sources" or "repository"))
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

    private async Task LoadRepositoryAsync()
    {
        SetRepositoryBusy(true);
        try
        {
            RepositoryConfiguration configuration = await _service.GetRepositoryAsync();
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
            _repositoryLoaded = true;
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

    private void SetRepositoryBusy(bool busy)
    {
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
    }

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
