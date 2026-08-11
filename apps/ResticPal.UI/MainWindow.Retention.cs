using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>Retention page: snapshot keep counts and prune cadence.</summary>
public sealed partial class MainWindow
{
    private bool _retentionLoaded;
    private bool _retentionAppendOnly;
    private bool _retentionDailyLocked;
    private bool _retentionWeeklyLocked;
    private bool _retentionMonthlyLocked;
    private bool _retentionYearlyLocked;
    private bool _pruneIntervalLocked;

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

        await RunGuardedAsync("retention-save", async () =>
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
        }, SetRetentionBusy);
    }

    private Task LoadRetentionAsync() =>
        RunGuardedAsync("retention-load", async () =>
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
        }, SetRetentionBusy);

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
}
