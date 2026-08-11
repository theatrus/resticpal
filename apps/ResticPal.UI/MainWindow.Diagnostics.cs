using System.Collections.ObjectModel;
using Microsoft.UI.Xaml;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>Diagnostics page: sanitized service event log.</summary>
public sealed partial class MainWindow
{
    private bool _diagnosticsLoaded;

    public ObservableCollection<DiagnosticListItem> Diagnostics { get; } = new();

    private async void RefreshDiagnosticsButton_Click(object sender, RoutedEventArgs e)
    {
        await LoadDiagnosticsAsync();
    }

    private Task LoadDiagnosticsAsync() =>
        RunGuardedAsync("diagnostics-load", async () =>
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
        }, SetDiagnosticsBusy);

    private void SetDiagnosticsBusy(bool busy)
    {
        DiagnosticsProgress.IsActive = busy;
        RefreshDiagnosticsButton.IsEnabled = !busy;
    }
}
