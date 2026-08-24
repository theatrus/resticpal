using System.Collections.ObjectModel;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>History page: completed backup runs.</summary>
public sealed partial class MainWindow
{
    private bool _historyLoaded;

    public ObservableCollection<BackupRunListItem> BackupHistory { get; } = new();

    private async void RefreshHistoryButton_Click(object sender, RoutedEventArgs e)
    {
        await LoadHistoryAsync();
    }

    private async void ShowRunFailureDetailsButton_Click(object sender, RoutedEventArgs e)
    {
        if (sender is not Button button || button.Tag is not BackupRunListItem run)
        {
            return;
        }

        button.IsEnabled = false;
        try
        {
            await RunGuardedAsync($"history-failure-details-{run.Id}", async () =>
            {
                BackupRunFailureDetails details =
                    await _service.GetRunFailureDetailsAsync(run.Id);
                if (details.RunId != run.Id)
                {
                    throw new InvalidDataException(
                        "The backup service returned details for a different history entry.");
                }

                var pathList = new StackPanel { Spacing = 8 };
                foreach (string item in details.Items)
                {
                    pathList.Children.Add(new TextBlock
                    {
                        IsTextSelectionEnabled = true,
                        Text = item,
                        TextWrapping = TextWrapping.Wrap,
                    });
                }
                if (details.Items.Count == 0)
                {
                    pathList.Children.Add(new TextBlock
                    {
                        Text = "No safe path entries were retained for this run.",
                        TextWrapping = TextWrapping.Wrap,
                    });
                }

                var content = new StackPanel { Spacing = 12 };
                content.Children.Add(new TextBlock
                {
                    Text = BackupWarningPresentation.DialogSummary(
                        details.Items.Count,
                        details.Omitted),
                    TextWrapping = TextWrapping.Wrap,
                });
                content.Children.Add(new ScrollViewer
                {
                    Content = pathList,
                    MaxHeight = 420,
                    VerticalScrollBarVisibility = ScrollBarVisibility.Auto,
                });

                var dialog = new ContentDialog
                {
                    CloseButtonText = "Close",
                    Content = content,
                    DefaultButton = ContentDialogButton.Close,
                    Title = "Files restic could not back up",
                    XamlRoot = NavigationRoot.XamlRoot,
                };
                await dialog.ShowAsync();
            });
        }
        finally
        {
            button.IsEnabled = true;
        }
    }

    private Task LoadHistoryAsync() =>
        RunGuardedAsync("history-load", async () =>
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
        }, SetHistoryBusy);

    private void SetHistoryBusy(bool busy)
    {
        HistoryProgress.IsActive = busy;
        RefreshHistoryButton.IsEnabled = !busy;
    }
}
