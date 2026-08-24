namespace ResticPal.UI.Services;

/// <summary>Stable, testable copy for locally retained source-failure details.</summary>
internal static class BackupWarningPresentation
{
    public static string CountSummary(ulong count) => count == 1
        ? "1 source item could not be read."
        : $"{count:N0} source items could not be read.";

    public static string ViewButtonLabel(ulong count) => count switch
    {
        0 => "View source warning details",
        1 => "View file restic could not back up",
        _ => $"View {count:N0} files restic could not back up",
    };

    public static string DialogSummary(int retained, ulong omitted)
    {
        ulong retainedCount = (ulong)Math.Max(0, retained);
        ulong total = ulong.MaxValue - retainedCount < omitted
            ? ulong.MaxValue
            : retainedCount + omitted;
        string shown = total == 1
            ? "restic reported one source item it could not read."
            : $"restic reported {total:N0} source items it could not read.";
        return omitted == 0
            ? $"{shown} These paths are stored only on this PC."
            : $"{shown} {omitted:N0} additional or unsafe path entries were omitted by the local safety limit.";
    }
}
