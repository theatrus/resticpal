using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;

namespace ResticPal.UI;

public sealed partial class MainWindow : Window
{
    private readonly ResticPalServiceClient _service = new();

    public MainWindow()
    {
        InitializeComponent();
    }

    private async void NavigationView_Loaded(object sender, RoutedEventArgs e)
    {
        await RefreshStatusAsync();
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
        }
        catch (Exception exception)
        {
            StatusTitle.Text = "Service unavailable";
            StatusDescription.Text = "The resticpal service could not be reached.";
            StatusCardTitle.Text = "Not connected";
            StatusCardDescription.Text = "Start or repair the resticpal service, then reopen this window.";
            RunBackupButton.IsEnabled = false;
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
}
