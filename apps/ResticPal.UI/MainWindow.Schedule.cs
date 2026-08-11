using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>Schedule page: backup interval and power/network behavior.</summary>
public sealed partial class MainWindow
{
    private bool _scheduleLoaded;
    private bool _scheduleIntervalLocked;
    private bool _wakeGraceLocked;
    private bool _wakeLockTimeoutLocked;
    private bool _allowBatteryLocked;
    private bool _allowMeteredLocked;

    private async void SaveScheduleButton_Click(object sender, RoutedEventArgs e)
    {
        if (!TryReadWholeNumber(
                ScheduleIntervalBox,
                "Backup interval",
                1,
                8_760,
                out ulong intervalHours)
            || !TryReadDurationSeconds(
                WakeGraceBox,
                "Wake grace period",
                allowZero: true,
                out ulong wakeGraceSeconds)
            || !TryReadDurationSeconds(
                WakeLockTimeoutBox,
                "Wake-lock timeout",
                allowZero: false,
                out ulong wakeLockTimeoutSeconds))
        {
            return;
        }

        await RunGuardedAsync("schedule-save", async () =>
        {
            CommandResult result = await _service.UpdateScheduleAsync(
                _scheduleIntervalLocked ? null : checked((uint)intervalHours),
                _wakeGraceLocked ? null : wakeGraceSeconds,
                _wakeLockTimeoutLocked ? null : wakeLockTimeoutSeconds,
                _allowBatteryLocked ? null : AllowBatteryToggle.IsOn,
                _allowMeteredLocked ? null : AllowMeteredToggle.IsOn);
            ShowMessage(
                result.Accepted ? InfoBarSeverity.Success : InfoBarSeverity.Warning,
                result.Message);
            if (result.Accepted)
            {
                _scheduleLoaded = false;
                await LoadScheduleAsync();
                await RefreshStatusAsync();
            }
        }, SetScheduleBusy);
    }

    private Task LoadScheduleAsync() =>
        RunGuardedAsync("schedule-load", async () =>
        {
            ScheduleConfiguration configuration = await _service.GetScheduleAsync();
            ScheduleIntervalBox.Value = configuration.IntervalHours;
            WakeGraceBox.Value = configuration.WakeGraceSeconds / 60.0;
            WakeLockTimeoutBox.Value = configuration.WakeLockTimeoutSeconds / 60.0;
            AllowBatteryToggle.IsOn = configuration.AllowOnBattery;
            AllowMeteredToggle.IsOn = configuration.AllowMeteredNetwork;
            _scheduleIntervalLocked = configuration.IntervalHoursLocked;
            _wakeGraceLocked = configuration.WakeGraceSecondsLocked;
            _wakeLockTimeoutLocked = configuration.WakeLockTimeoutSecondsLocked;
            _allowBatteryLocked = configuration.AllowOnBatteryLocked;
            _allowMeteredLocked = configuration.AllowMeteredNetworkLocked;

            int lockedFields = new[]
            {
                _scheduleIntervalLocked,
                _wakeGraceLocked,
                _wakeLockTimeoutLocked,
                _allowBatteryLocked,
                _allowMeteredLocked,
            }.Count(value => value);
            SchedulePolicyMessage.IsOpen = lockedFields > 0;
            SchedulePolicyMessage.Message = lockedFields == 5
                ? "The backup schedule and power/network behavior are managed by your organization."
                : $"{lockedFields} schedule field{(lockedFields == 1 ? " is" : "s are")} managed by your organization.";
            _scheduleLoaded = true;
        }, SetScheduleBusy);

    private void SetScheduleBusy(bool busy)
    {
        ScheduleProgress.IsActive = busy;
        ScheduleIntervalBox.IsEnabled = !busy && !_scheduleIntervalLocked;
        WakeGraceBox.IsEnabled = !busy && !_wakeGraceLocked;
        WakeLockTimeoutBox.IsEnabled = !busy && !_wakeLockTimeoutLocked;
        AllowBatteryToggle.IsEnabled = !busy && !_allowBatteryLocked;
        AllowMeteredToggle.IsEnabled = !busy && !_allowMeteredLocked;
        SaveScheduleButton.IsEnabled = !busy && !(
            _scheduleIntervalLocked
            && _wakeGraceLocked
            && _wakeLockTimeoutLocked
            && _allowBatteryLocked
            && _allowMeteredLocked);
    }
}
