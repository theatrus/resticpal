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
    private ScheduleConfiguration? _loadedScheduleConfiguration;
    private string _loadedScheduleIntervalText = string.Empty;
    private string _loadedWakeGraceText = string.Empty;
    private string _loadedWakeLockTimeoutText = string.Empty;

    private async void SaveScheduleButton_Click(object sender, RoutedEventArgs e)
    {
        if (_loadedScheduleConfiguration is null)
        {
            return;
        }

        bool intervalChanged = ScheduleIntervalChanged();
        bool wakeGraceChanged = WakeGraceChanged();
        bool wakeLockTimeoutChanged = WakeLockTimeoutChanged();
        bool allowBatteryChanged = AllowBatteryChanged();
        bool allowMeteredChanged = AllowMeteredChanged();
        if (!intervalChanged
            && !wakeGraceChanged
            && !wakeLockTimeoutChanged
            && !allowBatteryChanged
            && !allowMeteredChanged)
        {
            SetScheduleBusy(false);
            return;
        }

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
                ConfigurationFieldDiff.ValueOrNull(
                    intervalChanged,
                    checked((uint)intervalHours)),
                ConfigurationFieldDiff.ValueOrNull(wakeGraceChanged, wakeGraceSeconds),
                ConfigurationFieldDiff.ValueOrNull(
                    wakeLockTimeoutChanged,
                    wakeLockTimeoutSeconds),
                ConfigurationFieldDiff.ValueOrNull(
                    allowBatteryChanged,
                    AllowBatteryToggle.IsOn),
                ConfigurationFieldDiff.ValueOrNull(
                    allowMeteredChanged,
                    AllowMeteredToggle.IsOn));
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
            SchedulePanel.UpdateLayout();
            _loadedScheduleIntervalText = ScheduleIntervalBox.Text;
            _loadedWakeGraceText = WakeGraceBox.Text;
            _loadedWakeLockTimeoutText = WakeLockTimeoutBox.Text;
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
            _loadedScheduleConfiguration = configuration;
            _scheduleLoaded = true;
        }, SetScheduleBusy);

    private bool ScheduleIntervalChanged()
    {
        if (_loadedScheduleConfiguration is not ScheduleConfiguration loaded)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _scheduleIntervalLocked,
            (ScheduleIntervalBox.Value, ScheduleIntervalBox.Text),
            ((double)loaded.IntervalHours, _loadedScheduleIntervalText));
    }

    private bool WakeGraceChanged()
    {
        if (_loadedScheduleConfiguration is not ScheduleConfiguration loaded)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _wakeGraceLocked,
            (WakeGraceBox.Value, WakeGraceBox.Text),
            (loaded.WakeGraceSeconds / 60.0, _loadedWakeGraceText));
    }

    private bool WakeLockTimeoutChanged()
    {
        if (_loadedScheduleConfiguration is not ScheduleConfiguration loaded)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _wakeLockTimeoutLocked,
            (WakeLockTimeoutBox.Value, WakeLockTimeoutBox.Text),
            (loaded.WakeLockTimeoutSeconds / 60.0, _loadedWakeLockTimeoutText));
    }

    private bool AllowBatteryChanged()
    {
        if (_loadedScheduleConfiguration is not ScheduleConfiguration loaded)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _allowBatteryLocked,
            AllowBatteryToggle.IsOn,
            loaded.AllowOnBattery);
    }

    private bool AllowMeteredChanged()
    {
        if (_loadedScheduleConfiguration is not ScheduleConfiguration loaded)
        {
            return false;
        }

        return ConfigurationFieldDiff.Changed(
            _allowMeteredLocked,
            AllowMeteredToggle.IsOn,
            loaded.AllowMeteredNetwork);
    }

    private bool ScheduleHasUnsavedChanges() =>
        ScheduleIntervalChanged()
        || WakeGraceChanged()
        || WakeLockTimeoutChanged()
        || AllowBatteryChanged()
        || AllowMeteredChanged();

    private void ScheduleNumberField_ValueChanged(
        NumberBox sender,
        NumberBoxValueChangedEventArgs args)
    {
        RefreshScheduleControlState();
    }

    private void ScheduleToggle_Toggled(object sender, RoutedEventArgs e)
    {
        RefreshScheduleControlState();
    }

    private void RefreshScheduleControlState()
    {
        if (SaveScheduleButton is not null)
        {
            SetScheduleBusy(false);
        }
    }

    private void SetScheduleBusy(bool busy)
    {
        bool operationBusy = busy || ConfigurationPageOperationActive("schedule-");
        bool controlsDisabled = _configurationEditGate.ControlsDisabled(
            operationBusy,
            baselineAvailable: _loadedScheduleConfiguration is not null);
        ScheduleProgress.IsActive = operationBusy;
        ScheduleIntervalBox.IsEnabled = !controlsDisabled && !_scheduleIntervalLocked;
        WakeGraceBox.IsEnabled = !controlsDisabled && !_wakeGraceLocked;
        WakeLockTimeoutBox.IsEnabled = !controlsDisabled && !_wakeLockTimeoutLocked;
        AllowBatteryToggle.IsEnabled = !controlsDisabled && !_allowBatteryLocked;
        AllowMeteredToggle.IsEnabled = !controlsDisabled && !_allowMeteredLocked;
        SaveScheduleButton.IsEnabled = !controlsDisabled && ScheduleHasUnsavedChanges();
    }
}
