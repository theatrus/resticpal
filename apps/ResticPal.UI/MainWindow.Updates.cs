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
    private bool _updateSettingsLoaded;
    private AvailableUpdate? _availableUpdate;
    private DownloadedUpdate? _downloadedUpdate;

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

    private Task LoadUpdateSettingsAsync() =>
        RunGuardedAsync("update-settings-load", async () =>
        {
            UpdateSettingsConfiguration configuration = await _service.GetUpdateSettingsAsync();
            AutomaticUpdatesToggle.IsOn = configuration.AutomaticInstall;
            _updateSettingsLoaded = true;
        }, busy => AutomaticUpdatesToggle.IsEnabled = !busy);

    private async void AutomaticUpdatesToggle_Toggled(object sender, RoutedEventArgs e)
    {
        if (!_updateSettingsLoaded)
        {
            return;
        }
        await RunGuardedAsync("update-settings-save", async () =>
        {
            CommandResult result = await _service.UpdateUpdateSettingsAsync(
                AutomaticUpdatesToggle.IsOn);
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            if (!result.Accepted)
            {
                _updateSettingsLoaded = false;
                await LoadUpdateSettingsAsync();
            }
        }, busy => AutomaticUpdatesToggle.IsEnabled = !busy);
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
