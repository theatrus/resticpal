using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>One exact repository snapshot available on the chosen local date.</summary>
public sealed class RestoreSnapshotListItem
{
    internal RestoreSnapshotListItem(RestoreSnapshot snapshot)
    {
        Snapshot = snapshot;
        DateTimeOffset localTime = snapshot.Time.ToLocalTime();
        DisplayName = string.IsNullOrWhiteSpace(snapshot.Hostname)
            ? $"{localTime:t} · {ShortSnapshotId(snapshot.Id)}"
            : $"{localTime:t} · {snapshot.Hostname} · {ShortSnapshotId(snapshot.Id)}";
    }

    internal RestoreSnapshot Snapshot { get; }

    public string DisplayName { get; }

    private static string ShortSnapshotId(string id) => id.Length > 12 ? id[..12] : id;
}
