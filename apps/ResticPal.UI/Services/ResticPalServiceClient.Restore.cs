using System.Text.Json;

namespace ResticPal.UI.Services;

internal sealed partial class ResticPalServiceClient
{
    public async Task<RestoreSettingsConfiguration> GetRestoreSettingsAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(
            new { type = "get_restore_settings" },
            cancellationToken);
        RequirePayloadType(payload, "restore_settings");
        JsonElement configuration = payload.GetProperty("configuration");
        bool enabledLocked = configuration.GetProperty("enabled_locked").GetBoolean();
        bool managed = configuration.TryGetProperty("managed", out JsonElement managedElement)
            && managedElement.ValueKind is JsonValueKind.True or JsonValueKind.False
                ? managedElement.GetBoolean()
                : enabledLocked;

        return new RestoreSettingsConfiguration(
            configuration.GetProperty("enabled").GetBoolean(),
            enabledLocked,
            managed);
    }

    public Task<CommandResult> UpdateRestoreSettingsAsync(
        bool enabled,
        CancellationToken cancellationToken = default) =>
        SendCommandAsync(
            new { type = "update_restore_settings", enabled },
            cancellationToken);

    public async Task<ulong> BeginRestoreSnapshotQueryAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(
            new { type = "begin_restore_snapshot_query" },
            cancellationToken);
        RequirePayloadType(payload, "restore_query_started");
        return payload.GetProperty("query_id").GetUInt64();
    }

    public async Task<ulong> BeginRestoreDirectoryQueryAsync(
        string snapshotId,
        string path,
        CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(snapshotId);
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        JsonElement payload = await SendAsync(
            new
            {
                type = "begin_restore_directory_query",
                snapshot_id = snapshotId,
                path,
            },
            cancellationToken);
        RequirePayloadType(payload, "restore_query_started");
        return payload.GetProperty("query_id").GetUInt64();
    }

    public async Task<RestoreQueryResult> GetRestoreQueryAsync(
        ulong queryId,
        uint offset = 0,
        ushort limit = 100,
        CancellationToken cancellationToken = default)
    {
        if (limit is 0 or > 100)
        {
            throw new ArgumentOutOfRangeException(nameof(limit), "Page size must be 1 through 100.");
        }

        JsonElement payload = await SendAsync(
            new
            {
                type = "get_restore_query",
                query_id = queryId,
                offset,
                limit,
            },
            cancellationToken);
        RequirePayloadType(payload, "restore_query");
        JsonElement result = payload.GetProperty("result");
        ulong responseQueryId = result.GetProperty("query_id").GetUInt64();
        if (responseQueryId != queryId)
        {
            throw new InvalidDataException("The service returned a different restore query.");
        }

        IReadOnlyList<RestoreSnapshot> snapshots =
            result.TryGetProperty("snapshots", out JsonElement snapshotsElement)
            && snapshotsElement.ValueKind == JsonValueKind.Array
                ? snapshotsElement.EnumerateArray().Select(ReadRestoreSnapshot).ToArray()
                : [];
        IReadOnlyList<RestoreDirectoryEntry> entries =
            result.TryGetProperty("entries", out JsonElement entriesElement)
            && entriesElement.ValueKind == JsonValueKind.Array
                ? entriesElement.EnumerateArray().Select(ReadRestoreDirectoryEntry).ToArray()
                : [];

        return new RestoreQueryResult(
            responseQueryId,
            result.GetProperty("kind").GetString() ?? "unknown",
            result.GetProperty("state").GetString() ?? "unknown",
            snapshots,
            entries,
            ReadOptionalUInt64(result, "total") ?? 0,
            ReadOptionalString(result, "message"));
    }

    public Task<CommandResult> CancelRestoreQueryAsync(
        ulong queryId,
        CancellationToken cancellationToken = default) =>
        SendCommandAsync(
            new { type = "cancel_restore_query", query_id = queryId },
            cancellationToken);

    public async Task<ulong> StartRestoreAsync(
        string snapshotId,
        string path,
        string destination,
        CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(snapshotId);
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        ArgumentException.ThrowIfNullOrWhiteSpace(destination);
        JsonElement payload = await SendAsync(
            new
            {
                type = "start_restore",
                snapshot_id = snapshotId,
                path,
                destination,
            },
            cancellationToken,
            TimeSpan.FromSeconds(10));
        RequirePayloadType(payload, "restore_started");
        return payload.GetProperty("job_id").GetUInt64();
    }

    public async Task<RestoreOperationStatus> GetRestoreStatusAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(
            new { type = "get_restore_status" },
            cancellationToken);
        if (payload.GetProperty("type").GetString() == "rejected"
            && ReadOptionalString(payload, "code") == "restore_disabled")
        {
            throw new RestoreAccessDisabledException(
                ReadOptionalString(payload, "message")
                ?? "File restoration is disabled by this PC's current settings or managed policy.");
        }
        RequirePayloadType(payload, "restore_status");
        JsonElement status = payload.GetProperty("status");

        return new RestoreOperationStatus(
            ReadOptionalUInt64(status, "job_id"),
            status.GetProperty("state").GetString() ?? "idle",
            ReadOptionalUInt64(status, "files_restored") ?? 0,
            ReadOptionalUInt64(status, "bytes_restored") ?? 0,
            ReadOptionalUInt64(status, "total_files"),
            ReadOptionalUInt64(status, "total_bytes"),
            ReadOptionalString(status, "destination"),
            ReadOptionalString(status, "message"));
    }

    public Task<CommandResult> CancelRestoreAsync(
        CancellationToken cancellationToken = default) =>
        SendCommandAsync(new { type = "cancel_restore" }, cancellationToken);

    private static RestoreSnapshot ReadRestoreSnapshot(JsonElement snapshot) =>
        new(
            snapshot.GetProperty("id").GetString()
                ?? throw new InvalidDataException("A restore snapshot did not include its ID."),
            snapshot.GetProperty("time").GetDateTimeOffset(),
            ReadOptionalString(snapshot, "hostname"),
            snapshot.TryGetProperty("paths", out JsonElement paths)
            && paths.ValueKind == JsonValueKind.Array
                ? ReadStrings(paths)
                : []);

    private static RestoreDirectoryEntry ReadRestoreDirectoryEntry(JsonElement entry) =>
        new(
            entry.GetProperty("name").GetString()
                ?? throw new InvalidDataException("A restore entry did not include its name."),
            entry.GetProperty("path").GetString()
                ?? throw new InvalidDataException("A restore entry did not include its path."),
            entry.GetProperty("node_type").GetString() ?? "unknown",
            ReadOptionalUInt64(entry, "size"),
            ReadOptionalDateTimeOffset(entry, "modified_at"));
}

internal sealed record RestoreSettingsConfiguration(
    bool Enabled,
    bool EnabledLocked,
    bool Managed);

internal sealed record RestoreSnapshot(
    string Id,
    DateTimeOffset Time,
    string? Hostname,
    IReadOnlyList<string> Paths);

internal sealed record RestoreDirectoryEntry(
    string Name,
    string Path,
    string Kind,
    ulong? Size,
    DateTimeOffset? ModifiedAt);

internal sealed record RestoreQueryResult(
    ulong QueryId,
    string Kind,
    string State,
    IReadOnlyList<RestoreSnapshot> Snapshots,
    IReadOnlyList<RestoreDirectoryEntry> Entries,
    ulong Total,
    string? Message);

internal sealed record RestoreOperationStatus(
    ulong? JobId,
    string State,
    ulong FilesRestored,
    ulong BytesRestored,
    ulong? TotalFiles,
    ulong? TotalBytes,
    string? Destination,
    string? Message);

internal sealed class RestoreAccessDisabledException(string message)
    : InvalidOperationException(message);
