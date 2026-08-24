using Microsoft.UI.Xaml;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>Presentation model for one completed backup run in the history list.</summary>
public sealed class BackupRunListItem
{
    internal BackupRunListItem(BackupRun run)
    {
        Id = run.Id;
        Headline = run.Outcome switch
        {
            "succeeded" => "Backup completed",
            "succeeded_with_warnings" => "Backup completed with warnings",
            "cancelled" => "Backup cancelled",
            _ => "Backup failed",
        };

        TimeSpan duration = run.CompletedAt >= run.StartedAt
            ? run.CompletedAt - run.StartedAt
            : TimeSpan.Zero;
        CompletedAtText = $"{run.CompletedAt.ToLocalTime():g} · {FormatDuration(duration)}";

        var summary = new List<string>();
        if (run.FilesProcessed is ulong files)
        {
            summary.Add($"{files:N0} files");
        }
        if (run.BytesProcessed is ulong bytes)
        {
            summary.Add($"{FormatBytes(bytes)} processed");
        }
        if (run.DataAdded is ulong added)
        {
            summary.Add($"{FormatBytes(added)} added");
        }
        Summary = summary.Count == 0
            ? "No aggregate file statistics were reported."
            : string.Join(" · ", summary);

        if (run.FailedItemCount > 0)
        {
            Detail = BackupWarningPresentation.CountSummary(run.FailedItemCount);
        }
        else if (!string.IsNullOrWhiteSpace(run.ErrorCode))
        {
            Detail = $"Sanitized error code: {run.ErrorCode}";
        }
        else if (!string.IsNullOrWhiteSpace(run.SnapshotId))
        {
            Detail = $"Snapshot {run.SnapshotId}";
        }
        else
        {
            Detail = "No additional details.";
        }

        bool isPartialSourceWarning = run.Outcome == "succeeded_with_warnings"
            && run.ErrorCode == "restic_partial_source";
        FailureDetailsVisibility = run.FailedItemCount > 0 || isPartialSourceWarning
            ? Visibility.Visible
            : Visibility.Collapsed;
        FailureDetailsButtonText = BackupWarningPresentation.ViewButtonLabel(run.FailedItemCount);
    }

    public ulong Id { get; }
    public string Headline { get; }
    public string CompletedAtText { get; }
    public string Summary { get; }
    public string Detail { get; }
    public Visibility FailureDetailsVisibility { get; }
    public string FailureDetailsButtonText { get; }

    private static string FormatDuration(TimeSpan duration)
    {
        if (duration.TotalHours >= 1)
        {
            return $"{(int)duration.TotalHours}h {duration.Minutes}m";
        }
        if (duration.TotalMinutes >= 1)
        {
            return $"{(int)duration.TotalMinutes}m {duration.Seconds}s";
        }
        return $"{Math.Max(0, (int)duration.TotalSeconds)}s";
    }

    private static string FormatBytes(ulong bytes)
    {
        string[] units = ["B", "KB", "MB", "GB", "TB"];
        double value = bytes;
        int unit = 0;
        while (value >= 1024 && unit < units.Length - 1)
        {
            value /= 1024;
            unit++;
        }
        return $"{value:0.#} {units[unit]}";
    }
}
