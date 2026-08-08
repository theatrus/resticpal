using System.Buffers.Binary;
using System.IO.Pipes;
using System.Text.Json;

namespace ResticPal.UI.Services;

internal sealed class ResticPalServiceClient
{
    private const int ProtocolVersion = 1;
    private const int MaxFrameBytes = 1024 * 1024;
    private static long _nextRequestId;

    public async Task<ServiceSnapshot> GetStatusAsync(CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync("get_status", cancellationToken);
        if (payload.GetProperty("type").GetString() != "status")
        {
            throw new InvalidDataException("The service did not return a status response.");
        }

        JsonElement status = payload.GetProperty("status");
        JsonElement state = status.GetProperty("state");
        string stateName = state.GetProperty("state").GetString() ?? "unknown";
        string repositoryMode = status.GetProperty("repository_mode").GetString() ?? "standard";
        string? repositoryName = status.GetProperty("repository_display_name").GetString();

        (string headline, string description, bool canRun) = stateName switch
        {
            "unconfigured" => (
                "Setup required",
                "Choose backup sources and connect a repository to begin protecting this PC.",
                false),
            "idle" or "succeeded" => (
                "Protected",
                DescribeRepository(repositoryName, repositoryMode),
                true),
            "waiting" => (
                "Backup waiting",
                "The service is waiting for its scheduling conditions.",
                true),
            "running" => (
                "Backup in progress",
                DescribeRepository(repositoryName, repositoryMode),
                false),
            "succeeded_with_warnings" => (
                "Protected with warnings",
                "The latest backup completed, but some files need attention.",
                true),
            "failed" => (
                "Backup needs attention",
                "Open diagnostics for the sanitized error and local service logs.",
                true),
            "cancelled" => (
                "Last backup cancelled",
                DescribeRepository(repositoryName, repositoryMode),
                true),
            "paused" => (
                "Backups paused",
                "Backup execution is currently disabled by policy.",
                false),
            _ => ("Unknown service state", stateName, false),
        };

        return new ServiceSnapshot(headline, description, canRun);
    }

    public async Task<CommandResult> RunBackupNowAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement payload = await SendAsync("run_backup_now", cancellationToken);
        string responseType = payload.GetProperty("type").GetString() ?? "rejected";
        string message = payload.GetProperty("message").GetString() ?? "No message was returned.";
        return new CommandResult(responseType == "accepted", message);
    }

    private static async Task<JsonElement> SendAsync(
        string command,
        CancellationToken cancellationToken)
    {
        long requestId = Interlocked.Increment(ref _nextRequestId);
        byte[] payload = JsonSerializer.SerializeToUtf8Bytes(new
        {
            protocol_version = ProtocolVersion,
            request_id = requestId,
            command = new { type = command },
        });
        if (payload.Length > MaxFrameBytes)
        {
            throw new InvalidDataException("The service request exceeds the IPC size limit.");
        }

        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(TimeSpan.FromSeconds(2));
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

    private static string DescribeRepository(string? repositoryName, string repositoryMode)
    {
        string name = string.IsNullOrWhiteSpace(repositoryName) ? "Configured repository" : repositoryName;
        return repositoryMode == "append_only"
            ? $"{name} · append-only, with retention managed by the server"
            : $"{name} · retention managed by this PC";
    }
}

internal sealed record ServiceSnapshot(string Headline, string Description, bool CanRunBackup);

internal sealed record CommandResult(bool Accepted, string Message);
