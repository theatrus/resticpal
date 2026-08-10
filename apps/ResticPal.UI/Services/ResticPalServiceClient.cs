using System.Buffers.Binary;
using System.IO.Pipes;
using System.Security.Cryptography;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace ResticPal.UI.Services;

internal sealed class ResticPalServiceClient
{
    private const int ProtocolVersion = 3;
    private const int MaxFrameBytes = 1024 * 1024;
    private static long _nextRequestId;

    public async Task<ServiceSnapshot> GetStatusAsync(CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(new { type = "get_status" }, cancellationToken);
        if (payload.GetProperty("type").GetString() != "status")
        {
            throw new InvalidDataException("The service did not return a status response.");
        }

        JsonElement status = payload.GetProperty("status");
        JsonElement state = status.GetProperty("state");
        string stateName = state.GetProperty("state").GetString() ?? "unknown";
        string? waitingReason = state.TryGetProperty("reason", out JsonElement reasonElement)
            ? reasonElement.GetString()
            : null;
        string repositoryMode = status.GetProperty("repository_mode").GetString() ?? "standard";
        string? repositoryName = status.GetProperty("repository_display_name").GetString();

        (string headline, string description, bool canRun, bool canCancel) = stateName switch
        {
            "unconfigured" => (
                "Setup required",
                "Choose backup sources and connect a repository to begin protecting this PC.",
                false,
                false),
            "idle" or "succeeded" => (
                "Protected",
                DescribeRepository(repositoryName, repositoryMode),
                true,
                false),
            "waiting" when waitingReason == "repository_validation" => (
                "Repository setup required",
                "Test the saved repository connection before backups can continue.",
                false,
                false),
            "waiting" => (
                "Backup waiting",
                "The service is waiting for its scheduling conditions.",
                true,
                false),
            "running" => (
                "Backup in progress",
                DescribeProgress(status, repositoryName, repositoryMode),
                false,
                true),
            "succeeded_with_warnings" => (
                "Protected with warnings",
                "The latest backup completed, but some files need attention.",
                true,
                false),
            "failed" => (
                "Backup needs attention",
                "Open diagnostics for the sanitized error and local service logs.",
                true,
                false),
            "cancelled" => (
                "Last backup cancelled",
                DescribeRepository(repositoryName, repositoryMode),
                true,
                false),
            "paused" => (
                "Backups paused",
                "Backup execution is currently disabled by policy.",
                false,
                false),
            _ => ("Unknown service state", stateName, false, false),
        };

        return new ServiceSnapshot(headline, description, canRun, canCancel);
    }

    public async Task<ManagementConfiguration> GetManagementAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(new { type = "get_management" }, cancellationToken);
        RequirePayloadType(payload, "management");
        JsonElement configuration = payload.GetProperty("configuration");
        return new ManagementConfiguration(
            configuration.GetProperty("mode").GetString() ?? "disabled",
            configuration.GetProperty("enrolled").GetBoolean(),
            ReadOptionalString(configuration, "device_id"),
            ReadOptionalString(configuration, "manifest_url"));
    }

    public async Task<CommandResult> EnrollAsync(
        string bootstrapUrl,
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(
            new { type = "enroll", bootstrap_url = bootstrapUrl },
            cancellationToken,
            TimeSpan.FromSeconds(45));
        return ReadCommandResult(payload);
    }

    public async Task<CommandResult> UnenrollAsync(
        CancellationToken cancellationToken = default)
    {
        return await SendCommandAsync(new { type = "unenroll" }, cancellationToken);
    }

    public async Task<IReadOnlyList<BackupRun>> GetRunHistoryAsync(
        ushort limit = 50,
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(
            new { type = "get_run_history", limit },
            cancellationToken);
        RequirePayloadType(payload, "run_history");
        return payload.GetProperty("runs")
            .EnumerateArray()
            .Select(run => new BackupRun(
                run.GetProperty("id").GetUInt64(),
                run.GetProperty("started_at").GetDateTimeOffset(),
                run.GetProperty("completed_at").GetDateTimeOffset(),
                run.GetProperty("outcome").GetString() ?? "failed",
                ReadOptionalString(run, "error_code"),
                ReadOptionalUInt64(run, "files_processed"),
                ReadOptionalUInt64(run, "bytes_processed"),
                ReadOptionalUInt64(run, "data_added"),
                ReadOptionalString(run, "snapshot_id")))
            .ToArray();
    }

    public async Task<CommandResult> RunBackupNowAsync(
        CancellationToken cancellationToken = default)
    {
        return await SendCommandAsync(new { type = "run_backup_now" }, cancellationToken);
    }

    public async Task<CommandResult> CancelBackupAsync(
        CancellationToken cancellationToken = default)
    {
        return await SendCommandAsync(new { type = "cancel_backup" }, cancellationToken);
    }

    public async Task<BackupSourcesConfiguration> GetBackupSourcesAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(new { type = "get_backup_sources" }, cancellationToken);
        RequirePayloadType(payload, "backup_sources");
        JsonElement configuration = payload.GetProperty("configuration");
        return new BackupSourcesConfiguration(
            ReadStrings(configuration.GetProperty("paths")),
            ReadStrings(configuration.GetProperty("exclusions")),
            configuration.GetProperty("paths_locked").GetBoolean(),
            configuration.GetProperty("exclusions_locked").GetBoolean());
    }

    public async Task<IReadOnlyList<DiscoveredBackupSource>> DiscoverBackupSourcesAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(
            new { type = "discover_backup_sources" },
            cancellationToken,
            TimeSpan.FromSeconds(10));
        RequirePayloadType(payload, "discovered_backup_sources");
        return payload.GetProperty("sources")
            .EnumerateArray()
            .Select(source => new DiscoveredBackupSource(
                source.GetProperty("profile_name").GetString() ?? "User",
                source.GetProperty("kind").GetString() ?? "folder",
                source.GetProperty("path").GetString() ?? string.Empty))
            .Where(source => !string.IsNullOrWhiteSpace(source.Path))
            .ToArray();
    }

    public async Task<CommandResult> UpdateBackupSourcesAsync(
        IReadOnlyCollection<string>? paths,
        IReadOnlyCollection<string>? exclusions,
        CancellationToken cancellationToken = default)
    {
        return await SendCommandAsync(
            new { type = "update_backup_sources", paths, exclusions },
            cancellationToken);
    }

    public async Task<RepositoryConfiguration> GetRepositoryAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(new { type = "get_repository" }, cancellationToken);
        RequirePayloadType(payload, "repository");
        JsonElement configuration = payload.GetProperty("configuration");
        var options = new Dictionary<string, string>(StringComparer.Ordinal);
        foreach (JsonProperty option in configuration.GetProperty("options").EnumerateObject())
        {
            options[option.Name] = option.Value.GetString() ?? string.Empty;
        }

        return new RepositoryConfiguration(
            configuration.GetProperty("display_name").GetString(),
            configuration.GetProperty("url").GetString(),
            configuration.GetProperty("mode").GetString() ?? "standard",
            options,
            ReadStrings(configuration.GetProperty("configured_secrets")).ToHashSet(StringComparer.Ordinal),
            ReadRepositoryOperationStatus(configuration.GetProperty("operation_status")),
            configuration.GetProperty("display_name_locked").GetBoolean(),
            configuration.GetProperty("url_locked").GetBoolean(),
            configuration.GetProperty("mode_locked").GetBoolean(),
            configuration.GetProperty("options_locked").GetBoolean(),
            configuration.GetProperty("secrets_locked").GetBoolean());
    }

    public async Task<CommandResult> UpdateRepositoryAsync(
        string? displayName,
        string? url,
        string? mode,
        IReadOnlyDictionary<string, string>? options,
        IReadOnlyCollection<RepositorySecretUpdate> secretUpdates,
        CancellationToken cancellationToken = default)
    {
        return await SendCommandAsync(
            new
            {
                type = "update_repository",
                display_name = displayName,
                url,
                mode,
                options,
                secret_updates = secretUpdates,
            },
            cancellationToken);
    }

    public async Task<CommandResult> ValidateRepositoryAsync(
        CancellationToken cancellationToken = default)
    {
        return await SendCommandAsync(new { type = "validate_repository" }, cancellationToken);
    }

    public async Task<CommandResult> InitializeRepositoryAsync(
        CancellationToken cancellationToken = default)
    {
        return await SendCommandAsync(new { type = "initialize_repository" }, cancellationToken);
    }

    public async Task<ScheduleConfiguration> GetScheduleAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(new { type = "get_schedule" }, cancellationToken);
        RequirePayloadType(payload, "schedule");
        JsonElement configuration = payload.GetProperty("configuration");
        return new ScheduleConfiguration(
            configuration.GetProperty("interval_hours").GetUInt32(),
            configuration.GetProperty("wake_grace_seconds").GetUInt64(),
            configuration.GetProperty("wake_lock_timeout_seconds").GetUInt64(),
            configuration.GetProperty("allow_on_battery").GetBoolean(),
            configuration.GetProperty("allow_metered_network").GetBoolean(),
            configuration.GetProperty("interval_hours_locked").GetBoolean(),
            configuration.GetProperty("wake_grace_seconds_locked").GetBoolean(),
            configuration.GetProperty("wake_lock_timeout_seconds_locked").GetBoolean(),
            configuration.GetProperty("allow_on_battery_locked").GetBoolean(),
            configuration.GetProperty("allow_metered_network_locked").GetBoolean());
    }

    public async Task<CommandResult> UpdateScheduleAsync(
        uint? intervalHours,
        ulong? wakeGraceSeconds,
        ulong? wakeLockTimeoutSeconds,
        bool? allowOnBattery,
        bool? allowMeteredNetwork,
        CancellationToken cancellationToken = default)
    {
        return await SendCommandAsync(
            new
            {
                type = "update_schedule",
                interval_hours = intervalHours,
                wake_grace_seconds = wakeGraceSeconds,
                wake_lock_timeout_seconds = wakeLockTimeoutSeconds,
                allow_on_battery = allowOnBattery,
                allow_metered_network = allowMeteredNetwork,
            },
            cancellationToken);
    }

    public async Task<RetentionConfiguration> GetRetentionAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(new { type = "get_retention" }, cancellationToken);
        RequirePayloadType(payload, "retention");
        JsonElement configuration = payload.GetProperty("configuration");
        return new RetentionConfiguration(
            configuration.GetProperty("repository_mode").GetString() ?? "standard",
            configuration.GetProperty("daily").GetUInt32(),
            configuration.GetProperty("weekly").GetUInt32(),
            configuration.GetProperty("monthly").GetUInt32(),
            configuration.GetProperty("yearly").GetUInt32(),
            configuration.GetProperty("prune_interval_days").GetUInt32(),
            configuration.GetProperty("daily_locked").GetBoolean(),
            configuration.GetProperty("weekly_locked").GetBoolean(),
            configuration.GetProperty("monthly_locked").GetBoolean(),
            configuration.GetProperty("yearly_locked").GetBoolean(),
            configuration.GetProperty("prune_interval_days_locked").GetBoolean(),
            ReadOptionalDateTimeOffset(configuration, "last_retention"),
            ReadOptionalDateTimeOffset(configuration, "last_prune"),
            ReadOptionalString(configuration, "last_error"));
    }

    public async Task<CommandResult> UpdateRetentionAsync(
        uint? daily,
        uint? weekly,
        uint? monthly,
        uint? yearly,
        uint? pruneIntervalDays,
        CancellationToken cancellationToken = default)
    {
        return await SendCommandAsync(
            new
            {
                type = "update_retention",
                daily,
                weekly,
                monthly,
                yearly,
                prune_interval_days = pruneIntervalDays,
            },
            cancellationToken);
    }

    public async Task<IReadOnlyList<DiagnosticRecord>> GetDiagnosticsAsync(
        ushort limit = 100,
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync(
            new { type = "get_diagnostics", limit },
            cancellationToken);
        RequirePayloadType(payload, "diagnostics");
        return payload.GetProperty("entries")
            .EnumerateArray()
            .Select(entry => new DiagnosticRecord(
                entry.GetProperty("timestamp").GetDateTimeOffset(),
                entry.GetProperty("level").GetString() ?? "information",
                entry.GetProperty("event_id").GetString() ?? "service.unknown",
                entry.GetProperty("message").GetString() ?? "Service event.",
                ReadOptionalString(entry, "code")))
            .ToArray();
    }

    private static async Task<CommandResult> SendCommandAsync(
        object command,
        CancellationToken cancellationToken)
    {
        JsonElement payload = await SendAsync(command, cancellationToken);
        return ReadCommandResult(payload);
    }

    private static CommandResult ReadCommandResult(JsonElement payload)
    {
        string responseType = payload.GetProperty("type").GetString() ?? "rejected";
        string message = payload.GetProperty("message").GetString() ?? "No message was returned.";
        return new CommandResult(responseType == "accepted", message);
    }

    private static async Task<JsonElement> SendAsync(
        object command,
        CancellationToken cancellationToken,
        TimeSpan? requestTimeout = null)
    {
        long requestId = Interlocked.Increment(ref _nextRequestId);
        byte[] payload = JsonSerializer.SerializeToUtf8Bytes(new
        {
            protocol_version = ProtocolVersion,
            request_id = requestId,
            command,
        });
        try
        {
            return await ExchangeAsync(payload, requestId, cancellationToken, requestTimeout);
        }
        finally
        {
            CryptographicOperations.ZeroMemory(payload);
        }
    }

    private static async Task<JsonElement> ExchangeAsync(
        byte[] payload,
        long requestId,
        CancellationToken cancellationToken,
        TimeSpan? requestTimeout)
    {
        if (payload.Length > MaxFrameBytes)
        {
            throw new InvalidDataException("The service request exceeds the IPC size limit.");
        }
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(requestTimeout ?? TimeSpan.FromSeconds(2));
        await using var pipe = new NamedPipeClientStream(
            ".",
            "ResticPal.v3",
            PipeDirection.InOut,
            PipeOptions.Asynchronous);
        await pipe.ConnectAsync(timeout.Token);

        byte[] header = new byte[sizeof(uint)];
        BinaryPrimitives.WriteUInt32LittleEndian(header, checked((uint)payload.Length));
        await pipe.WriteAsync(header, timeout.Token);
        await pipe.WriteAsync(payload, timeout.Token);
        await pipe.FlushAsync(timeout.Token);

        await pipe.ReadExactlyAsync(header, timeout.Token);
        uint responseLength = BinaryPrimitives.ReadUInt32LittleEndian(header);
        if (responseLength is 0 or > MaxFrameBytes)
        {
            throw new InvalidDataException($"The service returned an invalid frame size: {responseLength}.");
        }

        byte[] response = new byte[responseLength];
        await pipe.ReadExactlyAsync(response, timeout.Token);
        using JsonDocument document = JsonDocument.Parse(response);
        JsonElement root = document.RootElement;
        int responseVersion = root.GetProperty("protocol_version").GetInt32();
        long responseId = root.GetProperty("request_id").GetInt64();
        if (responseVersion != ProtocolVersion)
        {
            throw new InvalidDataException(
                $"Service protocol {responseVersion} is incompatible with UI protocol {ProtocolVersion}.");
        }
        if (responseId != requestId)
        {
            throw new InvalidDataException(
                $"Service response ID {responseId} did not match request ID {requestId}.");
        }

        // The service uses this one-byte transport acknowledgement instead of
        // FlushFileBuffers, which would let a connected client stall its IPC
        // request loop indefinitely. A complete response remains valid if the
        // peer closes before the best-effort acknowledgement is written.
        header[0] = 0;
        try
        {
            await pipe.WriteAsync(header.AsMemory(0, 1), timeout.Token);
        }
        catch (IOException)
        {
        }
        catch (OperationCanceledException)
        {
        }

        return root.GetProperty("payload").Clone();
    }

    private static void RequirePayloadType(JsonElement payload, string expected)
    {
        string actual = payload.GetProperty("type").GetString() ?? "rejected";
        if (actual == expected)
        {
            return;
        }

        string message = payload.TryGetProperty("message", out JsonElement messageElement)
            ? messageElement.GetString() ?? "The service rejected the request."
            : $"The service returned '{actual}' instead of '{expected}'.";
        throw new InvalidOperationException(message);
    }

    private static IReadOnlyList<string> ReadStrings(JsonElement array)
    {
        return array.EnumerateArray()
            .Select(value => value.GetString())
            .Where(value => !string.IsNullOrEmpty(value))
            .Cast<string>()
            .ToArray();
    }

    private static string? ReadOptionalString(JsonElement value, string propertyName)
    {
        return value.TryGetProperty(propertyName, out JsonElement property)
            && property.ValueKind == JsonValueKind.String
                ? property.GetString()
                : null;
    }

    private static ulong? ReadOptionalUInt64(JsonElement value, string propertyName)
    {
        return value.TryGetProperty(propertyName, out JsonElement property)
            && property.ValueKind == JsonValueKind.Number
                ? property.GetUInt64()
                : null;
    }

    private static DateTimeOffset? ReadOptionalDateTimeOffset(
        JsonElement value,
        string propertyName)
    {
        return value.TryGetProperty(propertyName, out JsonElement property)
            && property.ValueKind == JsonValueKind.String
            && property.TryGetDateTimeOffset(out DateTimeOffset timestamp)
                ? timestamp
                : null;
    }

    private static RepositoryOperationStatus ReadRepositoryOperationStatus(JsonElement status)
    {
        string state = status.GetProperty("state").GetString() ?? "not_run";
        string? operation = status.TryGetProperty("operation", out JsonElement operationElement)
            ? operationElement.GetString()
            : null;
        DateTimeOffset? completedAt = status.TryGetProperty("completed_at", out JsonElement completedElement)
            && completedElement.TryGetDateTimeOffset(out DateTimeOffset timestamp)
                ? timestamp
                : null;
        string? code = status.TryGetProperty("code", out JsonElement codeElement)
            ? codeElement.GetString()
            : null;
        return new RepositoryOperationStatus(state, operation, completedAt, code);
    }

    private static string DescribeRepository(string? repositoryName, string repositoryMode)
    {
        string name = string.IsNullOrWhiteSpace(repositoryName) ? "Configured repository" : repositoryName;
        return repositoryMode == "append_only"
            ? $"{name} · append-only, with retention managed by the server"
            : $"{name} · retention managed by this PC";
    }

    private static string DescribeProgress(
        JsonElement status,
        string? repositoryName,
        string repositoryMode)
    {
        if (!status.TryGetProperty("progress", out JsonElement progress))
        {
            return $"Preparing {DescribeRepository(repositoryName, repositoryMode)}";
        }

        string percent = progress.TryGetProperty("percent_done", out JsonElement percentElement)
            && percentElement.ValueKind == JsonValueKind.Number
            ? $"{percentElement.GetByte()}% complete"
            : "Backup in progress";
        ulong filesDone = progress.GetProperty("files_done").GetUInt64();
        ulong bytesDone = progress.GetProperty("bytes_done").GetUInt64();
        return $"{percent} · {filesDone:N0} files · {FormatBytes(bytesDone)} processed";
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

internal sealed record ServiceSnapshot(
    string Headline,
    string Description,
    bool CanRunBackup,
    bool CanCancelBackup);

internal sealed record CommandResult(bool Accepted, string Message);

internal sealed record ManagementConfiguration(
    string Mode,
    bool Enrolled,
    string? DeviceId,
    string? ManifestUrl);

internal sealed record BackupRun(
    ulong Id,
    DateTimeOffset StartedAt,
    DateTimeOffset CompletedAt,
    string Outcome,
    string? ErrorCode,
    ulong? FilesProcessed,
    ulong? BytesProcessed,
    ulong? DataAdded,
    string? SnapshotId);

internal sealed record BackupSourcesConfiguration(
    IReadOnlyList<string> Paths,
    IReadOnlyList<string> Exclusions,
    bool PathsLocked,
    bool ExclusionsLocked);

internal sealed record RepositoryConfiguration(
    string? DisplayName,
    string? Url,
    string Mode,
    IReadOnlyDictionary<string, string> Options,
    IReadOnlySet<string> ConfiguredSecrets,
    RepositoryOperationStatus OperationStatus,
    bool DisplayNameLocked,
    bool UrlLocked,
    bool ModeLocked,
    bool OptionsLocked,
    bool SecretsLocked);

internal sealed record RepositoryOperationStatus(
    string State,
    string? Operation,
    DateTimeOffset? CompletedAt,
    string? Code);

internal sealed record ScheduleConfiguration(
    uint IntervalHours,
    ulong WakeGraceSeconds,
    ulong WakeLockTimeoutSeconds,
    bool AllowOnBattery,
    bool AllowMeteredNetwork,
    bool IntervalHoursLocked,
    bool WakeGraceSecondsLocked,
    bool WakeLockTimeoutSecondsLocked,
    bool AllowOnBatteryLocked,
    bool AllowMeteredNetworkLocked);

internal sealed record RetentionConfiguration(
    string RepositoryMode,
    uint Daily,
    uint Weekly,
    uint Monthly,
    uint Yearly,
    uint PruneIntervalDays,
    bool DailyLocked,
    bool WeeklyLocked,
    bool MonthlyLocked,
    bool YearlyLocked,
    bool PruneIntervalDaysLocked,
    DateTimeOffset? LastRetention,
    DateTimeOffset? LastPrune,
    string? LastError);

internal sealed record DiagnosticRecord(
    DateTimeOffset Timestamp,
    string Level,
    string EventId,
    string Message,
    string? Code);

internal sealed record RepositorySecretUpdate(
    [property: JsonPropertyName("action")] string Action,
    [property: JsonPropertyName("variable")] string Variable,
    [property: JsonPropertyName("value"),
        JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] string? Value)
{
    public static RepositorySecretUpdate Set(string variable, string value) =>
        new("set", variable, value);

    public static RepositorySecretUpdate Remove(string variable) =>
        new("remove", variable, null);
}

internal sealed record DiscoveredBackupSource(string ProfileName, string Kind, string Path)
{
    public string DisplayName => $"{ProfileName} · {Kind.Replace('_', ' ')}";
}
