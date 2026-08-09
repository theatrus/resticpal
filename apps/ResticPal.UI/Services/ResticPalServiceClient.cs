using System.Buffers.Binary;
using System.IO.Pipes;
using System.Security.Cryptography;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace ResticPal.UI.Services;

internal sealed class ResticPalServiceClient
{
    private const int ProtocolVersion = 1;
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

    private static async Task<CommandResult> SendCommandAsync(
        object command,
        CancellationToken cancellationToken)
    {
        JsonElement payload = await SendAsync(command, cancellationToken);
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
            "ResticPal.v1",
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
    bool DisplayNameLocked,
    bool UrlLocked,
    bool ModeLocked,
    bool OptionsLocked,
    bool SecretsLocked);

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
