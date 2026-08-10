using System.Reflection;
using NetSparkleUpdater;
using NetSparkleUpdater.Enums;
using NetSparkleUpdater.Events;
using NetSparkleUpdater.SignatureVerifiers;

namespace ResticPal.UI.Services;

internal sealed class ResticPalUpdateService : IDisposable
{
    private readonly SparkleUpdater _updater;
    private bool _disposed;

    internal ResticPalUpdateService()
    {
        _updater = new SparkleUpdater(
            UpdateTrust.AppCastUrl,
            new Ed25519Checker(SecurityMode.Strict, UpdateTrust.PublicKey))
        {
            UIFactory = null,
            RelaunchAfterUpdate = false,
            UserInteractionMode = UserInteractionMode.NotSilent,
        };
    }

    internal static string InstalledVersion
    {
        get
        {
            string? version = Assembly.GetExecutingAssembly()
                .GetCustomAttribute<AssemblyInformationalVersionAttribute>()?
                .InformationalVersion;
            return string.IsNullOrWhiteSpace(version) ? "unknown" : version;
        }
    }

    internal async Task<UpdateCheckResult> CheckAsync(CancellationToken cancellationToken = default)
    {
        ThrowIfDisposed();
        cancellationToken.ThrowIfCancellationRequested();
        UpdateInfo result = await _updater.CheckForUpdatesQuietly(ignoreSkippedVersions: true);
        cancellationToken.ThrowIfCancellationRequested();

        return result.Status switch
        {
            UpdateStatus.UpdateAvailable when result.Updates.Count > 0 =>
                UpdateCheckResult.Available(new AvailableUpdate(result.Updates[0])),
            UpdateStatus.UpdateNotAvailable => UpdateCheckResult.Current,
            UpdateStatus.UserSkipped => UpdateCheckResult.Current,
            _ => UpdateCheckResult.Unavailable,
        };
    }

    internal async Task<DownloadedUpdate> DownloadAsync(
        AvailableUpdate update,
        IProgress<int>? progress = null,
        CancellationToken cancellationToken = default)
    {
        ThrowIfDisposed();
        var completion = new TaskCompletionSource<string>(
            TaskCreationOptions.RunContinuationsAsynchronously);

        void DownloadFinished(AppCastItem item, string path)
        {
            if (ReferenceEquals(item, update.Item))
            {
                completion.TrySetResult(path);
            }
        }

        void DownloadFailed(AppCastItem item, string? _, Exception exception)
        {
            if (ReferenceEquals(item, update.Item))
            {
                completion.TrySetException(new InvalidOperationException(
                    "The update could not be downloaded or its signature was invalid.",
                    exception));
            }
        }

        void DownloadProgress(object? _, AppCastItem item, ItemDownloadProgressEventArgs args)
        {
            if (ReferenceEquals(item, update.Item))
            {
                progress?.Report(args.ProgressPercentage);
            }
        }

        _updater.DownloadFinished += DownloadFinished;
        _updater.DownloadHadError += DownloadFailed;
        _updater.DownloadMadeProgress += DownloadProgress;
        using CancellationTokenRegistration registration = cancellationToken.Register(() =>
        {
            _updater.CancelFileDownload();
            completion.TrySetCanceled(cancellationToken);
        });

        try
        {
            await _updater.InitAndBeginDownload(update.Item);
            string path = await completion.Task;
            return new DownloadedUpdate(update, path);
        }
        finally
        {
            _updater.DownloadFinished -= DownloadFinished;
            _updater.DownloadHadError -= DownloadFailed;
            _updater.DownloadMadeProgress -= DownloadProgress;
        }
    }

    internal async Task InstallAsync(DownloadedUpdate update, Action closeApplication)
    {
        ThrowIfDisposed();
        ArgumentNullException.ThrowIfNull(closeApplication);

        string? failure = null;
        bool InstallFailed(InstallUpdateFailureReason reason, string? _)
        {
            failure = reason switch
            {
                InstallUpdateFailureReason.InvalidSignature =>
                    "The downloaded update signature is invalid.",
                InstallUpdateFailureReason.FileNotFound =>
                    "The downloaded update is no longer available.",
                InstallUpdateFailureReason.CanceledByUserViaEvent =>
                    "Starting the installer was cancelled.",
                _ => "The Windows installer could not be started.",
            };
            return true;
        }
        void CloseApplication() => closeApplication();

        _updater.InstallUpdateFailed += InstallFailed;
        _updater.CloseApplication += CloseApplication;
        try
        {
            await _updater.InstallUpdate(update.Update.Item, update.Path);
            if (failure is not null)
            {
                throw new InvalidOperationException(failure);
            }
        }
        finally
        {
            _updater.InstallUpdateFailed -= InstallFailed;
            _updater.CloseApplication -= CloseApplication;
        }
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _updater.Dispose();
        _disposed = true;
    }

    private void ThrowIfDisposed()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
    }
}

internal sealed class AvailableUpdate
{
    internal AvailableUpdate(AppCastItem item)
    {
        Item = item;
    }

    internal AppCastItem Item { get; }
    internal string Version => Item.Version ?? "unknown";
    internal ulong? Size => Item.UpdateSize > 0 ? checked((ulong)Item.UpdateSize) : null;
}

internal sealed record DownloadedUpdate(AvailableUpdate Update, string Path);

internal sealed record UpdateCheckResult(UpdateCheckStatus Status, AvailableUpdate? Update)
{
    internal static UpdateCheckResult Current { get; } =
        new(UpdateCheckStatus.Current, null);
    internal static UpdateCheckResult Unavailable { get; } =
        new(UpdateCheckStatus.Unavailable, null);
    internal static UpdateCheckResult Available(AvailableUpdate update) =>
        new(UpdateCheckStatus.Available, update);
}

internal enum UpdateCheckStatus
{
    Current,
    Available,
    Unavailable,
}
