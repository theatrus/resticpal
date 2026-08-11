using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>Repository page: connection settings, credentials, and operations.</summary>
public sealed partial class MainWindow
{
    private bool _repositoryLoaded;
    private bool _repositoryDisplayNameLocked;
    private bool _repositoryUrlLocked;
    private bool _repositoryModeLocked;
    private bool _repositoryOptionsLocked;
    private bool _repositorySecretsLocked;
    private bool _repositoryBusy;
    private bool _applyingRepositoryConfiguration;
    private bool _repositoryDirty;
    private IReadOnlySet<string> _configuredRepositorySecrets = new HashSet<string>();

    private async void SaveRepositoryButton_Click(object sender, RoutedEventArgs e)
    {
        if (!TryReadRepositoryOptions(out IReadOnlyDictionary<string, string> options, out string error))
        {
            ShowMessage(InfoBarSeverity.Warning, error);
            return;
        }

        await RunGuardedAsync("repository-save", async () =>
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
        }, SetRepositoryBusy);
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

    private Task RunRepositoryOperationAsync(bool initialize) =>
        RunGuardedAsync("repository-operation", async () =>
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
        }, SetRepositoryBusy);

    private Task LoadRepositoryAsync() =>
        RunGuardedAsync("repository-load", async () =>
        {
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
            catch
            {
                // Editing must not stay suppressed after a failed load.
                _applyingRepositoryConfiguration = false;
                throw;
            }
        }, SetRepositoryBusy);

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
}
