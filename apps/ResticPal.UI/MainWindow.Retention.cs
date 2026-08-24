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
    private RetentionConfiguration? _loadedRetentionConfiguration;
    private string _loadedRetentionDailyText = string.Empty;
    private string _loadedRetentionWeeklyText = string.Empty;
    private string _loadedRetentionMonthlyText = string.Empty;
    private string _loadedRetentionYearlyText = string.Empty;
    private string _loadedPruneIntervalText = string.Empty;

    private async void SaveRetentionButton_Click(object sender, RoutedEventArgs e)
    {
        if (_loadedRetentionConfiguration is null || _retentionAppendOnly)
        {
            return;
        }

        bool dailyChanged = RetentionDailyChanged();
        bool weeklyChanged = RetentionWeeklyChanged();
        bool monthlyChanged = RetentionMonthlyChanged();
        bool yearlyChanged = RetentionYearlyChanged();
        bool pruneIntervalChanged = PruneIntervalChanged();
        if (!dailyChanged
            && !weeklyChanged
            && !monthlyChanged
            && !yearlyChanged
            && !pruneIntervalChanged)
        {
            SetRetentionBusy(false);
            return;
        }

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
                ConfigurationFieldDiff.ValueOrNull(dailyChanged, checked((uint)daily)),
                ConfigurationFieldDiff.ValueOrNull(weeklyChanged, checked((uint)weekly)),
                ConfigurationFieldDiff.ValueOrNull(monthlyChanged, checked((uint)monthly)),
                ConfigurationFieldDiff.ValueOrNull(yearlyChanged, checked((uint)yearly)),
                ConfigurationFieldDiff.ValueOrNull(
                    pruneIntervalChanged,
                    checked((uint)pruneDays)));
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
            RetentionPanel.UpdateLayout();
            _loadedRetentionDailyText = RetentionDailyBox.Text;
            _loadedRetentionWeeklyText = RetentionWeeklyBox.Text;
            _loadedRetentionMonthlyText = RetentionMonthlyBox.Text;
            _loadedRetentionYearlyText = RetentionYearlyBox.Text;
            _loadedPruneIntervalText = PruneIntervalBox.Text;
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
            _loadedRetentionConfiguration = configuration;
            _retentionLoaded = true;
        }, SetRetentionBusy);

    private bool RetentionDailyChanged()
    {
        if (_loadedRetentionConfiguration is not RetentionConfiguration loaded
            || _retentionAppendOnly)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _retentionDailyLocked,
            (RetentionDailyBox.Value, RetentionDailyBox.Text),
            ((double)loaded.Daily, _loadedRetentionDailyText));
    }

    private bool RetentionWeeklyChanged()
    {
        if (_loadedRetentionConfiguration is not RetentionConfiguration loaded
            || _retentionAppendOnly)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _retentionWeeklyLocked,
            (RetentionWeeklyBox.Value, RetentionWeeklyBox.Text),
            ((double)loaded.Weekly, _loadedRetentionWeeklyText));
    }

    private bool RetentionMonthlyChanged()
    {
        if (_loadedRetentionConfiguration is not RetentionConfiguration loaded
            || _retentionAppendOnly)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _retentionMonthlyLocked,
            (RetentionMonthlyBox.Value, RetentionMonthlyBox.Text),
            ((double)loaded.Monthly, _loadedRetentionMonthlyText));
    }

    private bool RetentionYearlyChanged()
    {
        if (_loadedRetentionConfiguration is not RetentionConfiguration loaded
            || _retentionAppendOnly)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _retentionYearlyLocked,
            (RetentionYearlyBox.Value, RetentionYearlyBox.Text),
            ((double)loaded.Yearly, _loadedRetentionYearlyText));
    }

    private bool PruneIntervalChanged()
    {
        if (_loadedRetentionConfiguration is not RetentionConfiguration loaded
            || _retentionAppendOnly)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _pruneIntervalLocked,
            (PruneIntervalBox.Value, PruneIntervalBox.Text),
            ((double)loaded.PruneIntervalDays, _loadedPruneIntervalText));
    }

    private bool RetentionHasUnsavedChanges() =>
        RetentionDailyChanged()
        || RetentionWeeklyChanged()
        || RetentionMonthlyChanged()
        || RetentionYearlyChanged()
        || PruneIntervalChanged();

    private void RetentionNumberField_ValueChanged(
        NumberBox sender,
        NumberBoxValueChangedEventArgs args)
    {
        RefreshRetentionControlState();
    }

    private void RefreshRetentionControlState()
    {
        if (SaveRetentionButton is not null)
        {
            SetRetentionBusy(false);
        }
    }

    private void SetRetentionBusy(bool busy)
    {
        bool operationBusy = busy || ConfigurationPageOperationActive("retention-");
        bool controlsDisabled = _configurationEditGate.ControlsDisabled(
            operationBusy,
            baselineAvailable: _loadedRetentionConfiguration is not null);
        RetentionProgress.IsActive = operationBusy;
        RetentionDailyBox.IsEnabled =
            !controlsDisabled && !_retentionAppendOnly && !_retentionDailyLocked;
        RetentionWeeklyBox.IsEnabled =
            !controlsDisabled && !_retentionAppendOnly && !_retentionWeeklyLocked;
        RetentionMonthlyBox.IsEnabled =
            !controlsDisabled && !_retentionAppendOnly && !_retentionMonthlyLocked;
        RetentionYearlyBox.IsEnabled =
            !controlsDisabled && !_retentionAppendOnly && !_retentionYearlyLocked;
        PruneIntervalBox.IsEnabled =
            !controlsDisabled && !_retentionAppendOnly && !_pruneIntervalLocked;
        SaveRetentionButton.IsEnabled =
            !controlsDisabled && !_retentionAppendOnly && RetentionHasUnsavedChanges();
    }
}
