using System.Globalization;

namespace ResticPal.UI.Services;

/// <summary>
/// Formats repository snapshot paths and restore progress without exposing
/// repository credentials or treating a snapshot path as a local filesystem path.
/// </summary>
internal static class RestorePresentation
{
    internal static string NormalizeSnapshotPath(string? path)
    {
        if (string.IsNullOrWhiteSpace(path))
        {
            return "/";
        }

        string trimmed = path.Trim('/');
        return trimmed.Length == 0 ? "/" : $"/{trimmed}";
    }

    internal static IReadOnlyList<RestoreBreadcrumb> Breadcrumbs(string? path)
    {
        string normalized = NormalizeSnapshotPath(path);
        var breadcrumbs = new List<RestoreBreadcrumb> { new("Backup", "/") };
        if (normalized == "/")
        {
            return breadcrumbs;
        }

        string current = string.Empty;
        foreach (string segment in normalized.Split('/', StringSplitOptions.RemoveEmptyEntries))
        {
            current += $"/{segment}";
            breadcrumbs.Add(new RestoreBreadcrumb(segment, current));
        }

        return breadcrumbs;
    }

    internal static bool TrySnapshotSourceRoot(
        string? windowsPath,
        out string snapshotPath,
        out string displayName)
    {
        snapshotPath = string.Empty;
        displayName = string.Empty;
        if (string.IsNullOrWhiteSpace(windowsPath)
            || windowsPath.Length < 3
            || !char.IsAsciiLetter(windowsPath[0])
            || windowsPath[1] != ':'
            || windowsPath[2] is not ('\\' or '/')
            || windowsPath.Any(char.IsControl))
        {
            return false;
        }

        string relative = windowsPath[3..].TrimEnd('\\', '/');
        string[] segments = relative.Length == 0
            ? []
            : relative.Split(['\\', '/'], StringSplitOptions.None);
        if (segments.Any(segment =>
                segment.Length == 0
                || segment is "." or ".."
                || segment.Contains(':')))
        {
            return false;
        }

        // Restic preserves the drive letter's original case in both snapshot
        // metadata and repository paths. Its tree is case-sensitive, so a
        // source backed up as c:\... must be browsed as /c/..., not /C/....
        char drive = windowsPath[0];
        snapshotPath = segments.Length == 0
            ? $"/{drive}"
            : $"/{drive}/{string.Join('/', segments)}";
        displayName = segments.Length == 0
            ? $"{drive}:"
            : segments[^1];
        return true;
    }

    internal static IReadOnlyList<RestoreBreadcrumb> SourceBreadcrumbs(
        string path,
        string sourceRoot,
        string sourceDisplayName)
    {
        string normalized = NormalizeSnapshotPath(path);
        string normalizedRoot = NormalizeSnapshotPath(sourceRoot);
        if (!string.Equals(normalized, normalizedRoot, StringComparison.Ordinal)
            && !normalized.StartsWith($"{normalizedRoot}/", StringComparison.Ordinal))
        {
            return Breadcrumbs(path);
        }

        var breadcrumbs = new List<RestoreBreadcrumb>
        {
            new("Backup", "/"),
            new(sourceDisplayName, normalizedRoot),
        };
        string current = normalizedRoot;
        foreach (string segment in normalized[normalizedRoot.Length..]
                     .Split('/', StringSplitOptions.RemoveEmptyEntries))
        {
            current += $"/{segment}";
            breadcrumbs.Add(new RestoreBreadcrumb(segment, current));
        }

        return breadcrumbs;
    }

    internal static bool MatchesLocalDate(DateTimeOffset timestamp, DateTimeOffset selectedDate) =>
        timestamp.ToLocalTime().Date == selectedDate.ToLocalTime().Date;

    internal static string FormatBytes(ulong bytes)
    {
        string[] units = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
        double value = bytes;
        int unit = 0;
        while (value >= 1024 && unit < units.Length - 1)
        {
            value /= 1024;
            unit++;
        }

        return $"{value.ToString("0.#", CultureInfo.CurrentCulture)} {units[unit]}";
    }

    internal static string FormatProgress(ulong filesRestored, ulong bytesRestored) =>
        $"{filesRestored:N0} file{(filesRestored == 1 ? string.Empty : "s")} · "
        + $"{FormatBytes(bytesRestored)} restored";

    internal static string FormatStatusMessage(
        string? message,
        ulong filesRestored,
        ulong bytesRestored)
    {
        string progress = FormatProgress(filesRestored, bytesRestored);
        if (string.IsNullOrWhiteSpace(message))
        {
            return progress;
        }

        return filesRestored == 0 && bytesRestored == 0
            ? message
            : $"{message} · {progress}";
    }

    internal static bool IsDirectory(string kind) =>
        string.Equals(kind, "dir", StringComparison.OrdinalIgnoreCase)
        || string.Equals(kind, "directory", StringComparison.OrdinalIgnoreCase);

    internal static bool IsActiveState(string state) =>
        state is "queued" or "starting" or "running" or "cancelling";

    internal static bool IsTerminalState(string state) =>
        state is "succeeded" or "completed" or "cancelled" or "failed";

    internal static string OperationTitle(string state, bool cancellationRequested = false) =>
        cancellationRequested && IsActiveState(state)
            ? "Cancelling restore"
            : state switch
            {
                "queued" or "starting" or "running" => "Restore in progress",
                "cancelling" => "Cancelling restore",
                "succeeded" or "completed" => "Restore completed and verified",
                "cancelled" => "Restore cancelled — partial files were kept",
                "failed" => "Restore could not be completed",
                _ => "Restore status",
            };
}

public sealed class RestoreBreadcrumb(string displayName, string path)
{
    public string DisplayName { get; } = displayName;

    public string Path { get; } = path;
}

/// <summary>
/// Advances by the actual returned row count, because the service can trim a
/// requested 100-row page to remain below the named-pipe frame limit.
/// </summary>
internal sealed class RestorePageAccumulator<T>
{
    private readonly List<T> _items = [];
    private ulong? _expectedTotal;

    internal uint Offset => checked((uint)_items.Count);

    internal IReadOnlyList<T> Items => _items;

    internal bool Add(IReadOnlyList<T> page, ulong total)
    {
        ArgumentNullException.ThrowIfNull(page);
        if (total > uint.MaxValue)
        {
            throw new InvalidDataException("The restore query exceeded the supported page range.");
        }
        if (_expectedTotal is ulong expected && expected != total)
        {
            throw new InvalidDataException("The service changed the restore result count between pages.");
        }
        if (page.Count > 100)
        {
            throw new InvalidDataException("The service returned an oversized restore result page.");
        }

        _expectedTotal = total;
        _items.AddRange(page);
        if ((ulong)_items.Count > total)
        {
            throw new InvalidDataException("The service returned more restore entries than expected.");
        }
        if ((ulong)_items.Count == total)
        {
            return true;
        }
        if (page.Count == 0)
        {
            throw new InvalidDataException("The restore query stopped before all entries were returned.");
        }

        return false;
    }
}
