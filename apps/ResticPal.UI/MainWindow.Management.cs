using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>Management page: enrollment state, onboarding, and first-run flow.</summary>
public sealed partial class MainWindow
{
    private bool _managementLoaded;
    private bool _managedDevice;

    private Task LoadManagementAsync() =>
        RunGuardedAsync("management-load", async () =>
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
        }, SetManagementBusy);

    private async void EnrollButton_Click(object sender, RoutedEventArgs e)
    {
        string bootstrapUrl = BootstrapUrlBox.Password.Trim();
        if (string.IsNullOrWhiteSpace(bootstrapUrl))
        {
            ShowMessage(InfoBarSeverity.Warning, "Paste the one-time bootstrap URL first.");
            return;
        }
        await RunGuardedAsync("management-enroll", async () =>
        {
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
                    await RefreshStatusAsync(synchronizeConfiguration: true);
                }
            }
            finally
            {
                // The one-time bootstrap URL never lingers in the input.
                BootstrapUrlBox.Password = string.Empty;
            }
        }, SetManagementBusy);
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
        await RunGuardedAsync("management-unenroll", async () =>
        {
            CommandResult result = await _service.UnenrollAsync();
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            if (result.Accepted)
            {
                _managementLoaded = false;
                await LoadManagementAsync();
                await RefreshStatusAsync(synchronizeConfiguration: true);
            }
        }, SetManagementBusy);
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
}
