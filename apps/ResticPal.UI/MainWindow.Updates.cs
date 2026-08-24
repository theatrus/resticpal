using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>
/// Updates section: signed feed checks, download, and install. These handlers
/// keep their own try/catch blocks because failures are reported with
/// update-specific wording rather than the shared connection error.
/// </summary>
public sealed partial class MainWindow
{
    private bool _updateCheckAttempted;
    private bool _updateSettingsLoadAttempted;
    private DateTimeOffset _nextUpdateSettingsRecoveryAttempt = DateTimeOffset.MinValue;
    private bool _updateSettingsLoaded;
    private bool _automaticUpdatesLocked;
    private int _updateSettingsBusyScopeCount;
    private int _updateBusyScopeCount;
    private AvailableUpdate? _availableUpdate;
    private DownloadedUpdate? _downloadedUpdate;

    private async Task CheckForUpdatesAsync(bool userInitiated, bool force = false)
    {
        if (_updateCheckAttempted && !userInitiated && !force)
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
                    switch (UpdateInstallationDecision.Select(
                        _updateSettingsLoaded,
                        AutomaticUpdatesToggle.IsOn))
                    {
                        case AvailableUpdateAction.RequestAutomaticInstall:
                            await RequestAutomaticUpdateAsync(result.Update, userInitiated);
                            break;
                        case AvailableUpdateAction.OfferManualDownload:
                            UpdateStatusDescription.Text =
                                $"resticpal {result.Update.Version} is available. The appcast and installer signatures will both be verified before installation.";
                            DownloadUpdateButton.Visibility = Visibility.Visible;
                            break;
                        default:
                            UpdateStatusDescription.Text =
                                $"resticpal {result.Update.Version} is available, but resticpal could not determine the effective automatic-update policy. Reload Settings before installing.";
                            DownloadUpdateButton.Visibility = Visibility.Collapsed;
                            break;
                    }
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

    private async Task LoadUpdateSettingsAsync()
    {
        _updateSettingsLoadAttempted = true;
        _nextUpdateSettingsRecoveryAttempt = DateTimeOffset.UtcNow.AddSeconds(15);
        await RunGuardedAsync("update-settings-load", async () =>
        {
            _updateSettingsLoaded = false;
            RefreshAvailableUpdateControls();
            UpdateSettingsConfiguration configuration = await _service.GetUpdateSettingsAsync();
            ApplyUpdateSettings(configuration);

            if (configuration.AutomaticInstall && _availableUpdate is not null)
            {
                await RequestAutomaticUpdateAsync(_availableUpdate, userInitiated: false);
            }
        }, SetUpdateSettingsBusy);
    }

    private async void AutomaticUpdatesToggle_Toggled(object sender, RoutedEventArgs e)
    {
        if (!_updateSettingsLoaded)
        {
            return;
        }
        await RunGuardedAsync("update-settings-save", async () =>
        {
            CommandResult result;
            try
            {
                result = await _service.UpdateUpdateSettingsAsync(
                    AutomaticUpdatesToggle.IsOn);
            }
            catch
            {
                _updateSettingsLoaded = false;
                RefreshAvailableUpdateControls();
                await LoadUpdateSettingsAsync();
                throw;
            }
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            if (!result.Accepted)
            {
                _updateSettingsLoaded = false;
                await LoadUpdateSettingsAsync();
                return;
            }

            if (AutomaticUpdatesToggle.IsOn)
            {
                if (_availableUpdate is not null)
                {
                    await RequestAutomaticUpdateAsync(_availableUpdate, userInitiated: true);
                }
                else
                {
                    await CheckForUpdatesAsync(userInitiated: false, force: true);
                }
            }
            else
            {
                RefreshAvailableUpdateControls();
            }
        }, SetUpdateSettingsBusy);
    }

    private async void CheckForUpdatesButton_Click(object sender, RoutedEventArgs e)
    {
        await CheckForUpdatesAsync(userInitiated: true);
    }

    private async void DownloadUpdateButton_Click(object sender, RoutedEventArgs e)
    {
        AvailableUpdate? update = _availableUpdate;
        if (update is null
            || !_updateSettingsLoaded
            || _updateSettingsBusyScopeCount > 0
            || AutomaticUpdatesToggle.IsOn)
        {
            return;
        }

        if (!await RevalidateManualUpdateModeAsync(update))
        {
            return;
        }

        var confirmation = new ContentDialog
        {
            Title = $"Download resticpal {update.Version}?",
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

        // A managed policy may have changed while the dialog was open. Do not
        // begin a prompted download unless the effective setting is still
        // explicitly manual when the action starts.
        if (!await RevalidateManualUpdateModeAsync(update))
        {
            return;
        }

        SetUpdateBusy(true);
        UpdateDownloadProgress.Value = 0;
        UpdateDownloadProgress.Visibility = Visibility.Visible;
        UpdateStatusDescription.Text = $"Downloading resticpal {update.Version}…";
        try
        {
            var progress = new Progress<int>(percentage =>
            {
                UpdateDownloadProgress.Value = Math.Clamp(percentage, 0, 100);
                UpdateStatusDescription.Text =
                    $"Downloading resticpal {update.Version}… {percentage}%";
            });
            _downloadedUpdate = await _updates.DownloadAsync(update, progress);
            UpdateDownloadProgress.Value = 100;
            UpdateStatusDescription.Text =
                $"resticpal {update.Version} is downloaded and its Ed25519 signature is valid.";
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
        DownloadedUpdate? downloadedUpdate = _downloadedUpdate;
        if (downloadedUpdate is null
            || !_updateSettingsLoaded
            || _updateSettingsBusyScopeCount > 0
            || AutomaticUpdatesToggle.IsOn)
        {
            return;
        }

        if (!await RevalidateManualUpdateModeAsync(downloadedUpdate.Update))
        {
            return;
        }

        var confirmation = new ContentDialog
        {
            Title = $"Install resticpal {downloadedUpdate.Update.Version}?",
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

        if (!await RevalidateManualUpdateModeAsync(downloadedUpdate.Update))
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
                downloadedUpdate,
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

    private async Task RequestAutomaticUpdateAsync(
        AvailableUpdate update,
        bool userInitiated)
    {
        _downloadedUpdate = null;
        DownloadUpdateButton.Visibility = Visibility.Collapsed;
        InstallUpdateButton.Visibility = Visibility.Collapsed;
        UpdateDownloadProgress.Visibility = Visibility.Collapsed;
        SetUpdateBusy(true);
        UpdateStatusDescription.Text =
            $"Starting the silent background update to resticpal {update.Version}…";
        try
        {
            CommandResult result = await _service.InstallUpdateAsync(update.SignedPackage);
            switch (UpdateInstallationDecision.AfterAutomaticRequest(result.Accepted))
            {
                case AutomaticUpdateRequestAction.Complete:
                    _availableUpdate = null;
                    UpdateStatusDescription.Text =
                        $"resticpal {update.Version} is downloading and will install silently in the background. The tray and this window may restart when it is ready.";
                    if (userInitiated)
                    {
                        ShowMessage(
                            InfoBarSeverity.Informational,
                            "The protected service is downloading and installing the signed update; no further confirmation is needed.");
                    }
                    break;
                case AutomaticUpdateRequestAction.RetrySilently:
                    UpdateStatusDescription.Text =
                        $"The background update could not start yet: {result.Message} Automatic mode remains enabled and resticpal will retry.";
                    if (userInitiated)
                    {
                        ShowMessage(InfoBarSeverity.Warning, result.Message);
                    }
                    break;
            }
        }
        catch (Exception exception)
        {
            UpdateStatusDescription.Text =
                "The protected service could not start the background update. Automatic mode remains enabled and resticpal will retry.";
            if (userInitiated)
            {
                ShowMessage(
                    InfoBarSeverity.Warning,
                    exception is OperationCanceledException
                        ? "The automatic update request timed out."
                        : "The protected service is unavailable right now.");
            }
        }
        finally
        {
            SetUpdateBusy(false);
        }
    }

    private void ApplyUpdateSettings(UpdateSettingsConfiguration configuration)
    {
        _automaticUpdatesLocked = configuration.AutomaticInstallLocked;
        AutomaticUpdatesToggle.IsOn = configuration.AutomaticInstall;
        _updateSettingsLoaded = true;
        _nextUpdateSettingsRecoveryAttempt = DateTimeOffset.MaxValue;
        UpdatePolicyMessage.Message = configuration.AutomaticInstall
            ? "Silent automatic installation is required by your organization's policy."
            : "Automatic installation is disabled by your organization's policy.";
        UpdatePolicyMessage.IsOpen = configuration.AutomaticInstallLocked;
        RefreshAvailableUpdateControls();
    }

    private async Task<bool> RevalidateManualUpdateModeAsync(AvailableUpdate update)
    {
        SetUpdateSettingsBusy(true);
        _updateSettingsLoaded = false;
        RefreshAvailableUpdateControls();
        try
        {
            UpdateSettingsConfiguration configuration = await _service.GetUpdateSettingsAsync();
            ApplyUpdateSettings(configuration);
            if (!configuration.AutomaticInstall)
            {
                return true;
            }

            await RequestAutomaticUpdateAsync(update, userInitiated: true);
            return false;
        }
        catch (Exception exception)
        {
            _updateSettingsLoaded = false;
            _nextUpdateSettingsRecoveryAttempt = DateTimeOffset.UtcNow.AddSeconds(15);
            RefreshAvailableUpdateControls();
            UpdateStatusDescription.Text =
                "resticpal could not verify the effective update policy, so no prompted update action was started.";
            ShowMessage(
                InfoBarSeverity.Warning,
                exception is OperationCanceledException
                    ? "The update-policy check timed out."
                    : "The protected service is unavailable right now.");
            return false;
        }
        finally
        {
            SetUpdateSettingsBusy(false);
        }
    }

    private void RefreshAvailableUpdateControls()
    {
        bool manualMode = _updateSettingsLoaded && !AutomaticUpdatesToggle.IsOn;
        DownloadUpdateButton.Visibility = manualMode && _availableUpdate is not null
            ? Visibility.Visible
            : Visibility.Collapsed;
        InstallUpdateButton.Visibility = manualMode && _downloadedUpdate is not null
            ? Visibility.Visible
            : Visibility.Collapsed;
        RefreshUpdateControlState();
    }

    private void SetUpdateSettingsBusy(bool busy)
    {
        _updateSettingsBusyScopeCount = busy
            ? checked(_updateSettingsBusyScopeCount + 1)
            : Math.Max(0, _updateSettingsBusyScopeCount - 1);
        RefreshUpdateControlState();
    }

    private void SetUpdateBusy(bool busy)
    {
        _updateBusyScopeCount = busy
            ? checked(_updateBusyScopeCount + 1)
            : Math.Max(0, _updateBusyScopeCount - 1);
        RefreshUpdateControlState();
    }

    private void RefreshUpdateControlState()
    {
        bool updateBusy = _updateBusyScopeCount > 0;
        bool reloadBusy = _configurationEditGate.ReloadInProgress;
        UpdateProgress.IsActive = updateBusy;
        CheckForUpdatesButton.IsEnabled = _updateSettingsLoaded
            && _updateSettingsBusyScopeCount == 0
            && !updateBusy
            && !reloadBusy;
        AutomaticUpdatesToggle.IsEnabled = _updateSettingsLoaded
            && !_automaticUpdatesLocked
            && _updateSettingsBusyScopeCount == 0
            && !updateBusy
            && !reloadBusy;
        bool manualMode = _updateSettingsLoaded && !AutomaticUpdatesToggle.IsOn;
        DownloadUpdateButton.IsEnabled = !updateBusy
            && !reloadBusy
            && _updateSettingsBusyScopeCount == 0
            && manualMode
            && _availableUpdate is not null;
        InstallUpdateButton.IsEnabled = !updateBusy
            && !reloadBusy
            && _updateSettingsBusyScopeCount == 0
            && manualMode
            && _downloadedUpdate is not null;
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
}
