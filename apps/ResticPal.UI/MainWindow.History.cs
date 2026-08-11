using System.Collections.ObjectModel;
using Microsoft.UI.Xaml;
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
