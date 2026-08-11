using System.IO;
using System.Reflection;
using NetSparkleUpdater;
using NetSparkleUpdater.Enums;
using NetSparkleUpdater.Events;
using NetSparkleUpdater.SignatureVerifiers;

namespace ResticPal.UI.Services;

internal sealed class ResticPalUpdateService : IDisposable
{
    private readonly IReadOnlyList<SparkleUpdater> _updaters;
    private bool _disposed;

    internal ResticPalUpdateService()
    {
        _updaters = UpdateTrust.AppCastUrls.Select(CreateUpdater).ToArray();
    }

    private static SparkleUpdater CreateUpdater(string appCastUrl)
    {
        return new SparkleUpdater(
            appCastUrl,
            new Ed25519Checker(SecurityMode.Strict, UpdateTrust.PublicKey))
        {
            UIFactory = null,
            RelaunchAfterUpdate = false,
            UserInteractionMode = UserInteractionMode.NotSilent,
            CustomInstallerArguments = "/qn /norestart",
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
        foreach (SparkleUpdater updater in _updaters)
        {
            UpdateInfo result;
            try
            {
                result = await updater.CheckForUpdatesQuietly(ignoreSkippedVersions: true);
            }
            catch when (!cancellationToken.IsCancellationRequested)
            {
                continue;
            }
            cancellationToken.ThrowIfCancellationRequested();

            switch (result.Status)
            {
                case UpdateStatus.UpdateAvailable when result.Updates.Count > 0:
                    return UpdateCheckResult.Available(
                        new AvailableUpdate(updater, result.Updates[0]));
                case UpdateStatus.UpdateNotAvailable:
                case UpdateStatus.UserSkipped:
                    return UpdateCheckResult.Current;
            }
        }
        return UpdateCheckResult.Unavailable;
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

        SparkleUpdater updater = update.Updater;
        updater.DownloadFinished += DownloadFinished;
        updater.DownloadHadError += DownloadFailed;
        updater.DownloadMadeProgress += DownloadProgress;
        using CancellationTokenRegistration registration = cancellationToken.Register(() =>
        {
            updater.CancelFileDownload();
            completion.TrySetCanceled(cancellationToken);
        });

        try
        {
            // NetSparkle follows GitHub's redirect before choosing a local
            // name. GitHub's release-object response does not reliably expose
            // the original Content-Disposition filename, so the default can
            // become an extensionless GUID. NetSparkle selects msiexec from
            // the downloaded path's extension; make the MSI identity explicit.
            updater.TmpDownloadFileNameWithExtension =
                $"resticpal-{NormalizeVersionForFileName(update.Version)}-x64-{Guid.NewGuid():N}.msi";
            await updater.InitAndBeginDownload(update.Item);
            string path = await completion.Task;
            if (!string.Equals(Path.GetExtension(path), ".msi", StringComparison.OrdinalIgnoreCase))
            {
                throw new InvalidOperationException(
                    "The signed update was not staged as a Windows Installer package.");
            }
            return new DownloadedUpdate(update, path);
        }
        finally
        {
            updater.DownloadFinished -= DownloadFinished;
            updater.DownloadHadError -= DownloadFailed;
            updater.DownloadMadeProgress -= DownloadProgress;
        }
    }

    private static string NormalizeVersionForFileName(string version)
    {
        return version.Length is > 0 and <= 32
            && version.All(character => char.IsAsciiDigit(character) || character == '.')
                ? version
                : "update";
    }

    internal async Task InstallAsync(DownloadedUpdate update, Action closeApplication)
    {
        ThrowIfDisposed();
        ArgumentNullException.ThrowIfNull(closeApplication);

        // The installer was verified when it downloaded, but it has since sat in
        // a user-writable temp directory while the user decided whether to
        // install. A non-elevated same-user process could have swapped it in
        // that window. Re-verify the pinned Ed25519 signature of the exact file
        // on disk before this elevated process launches it; an attacker without
        // the private key cannot produce a file that passes.
        VerifyDownloadedSignature(update);

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

        SparkleUpdater updater = update.Update.Updater;
        updater.InstallUpdateFailed += InstallFailed;
        updater.CloseApplication += CloseApplication;
        try
        {
            await updater.InstallUpdate(update.Update.Item, update.Path);
            if (failure is not null)
            {
                throw new InvalidOperationException(failure);
            }
        }
        finally
        {
            updater.InstallUpdateFailed -= InstallFailed;
            updater.CloseApplication -= CloseApplication;
        }
    }

    private static void VerifyDownloadedSignature(DownloadedUpdate update)
    {
        string signature = update.Update.Item.DownloadSignature ?? string.Empty;
        var checker = new Ed25519Checker(SecurityMode.Strict, UpdateTrust.PublicKey);
        if (!File.Exists(update.Path)
            || checker.VerifySignatureOfFile(signature, update.Path) != ValidationResult.Valid)
        {
            throw new InvalidOperationException(
                "The downloaded update failed signature verification and will not be installed.");
        }
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        foreach (SparkleUpdater updater in _updaters)
        {
            updater.Dispose();
        }
        _disposed = true;
    }

    private void ThrowIfDisposed()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
    }
}

internal sealed class AvailableUpdate
{
    internal AvailableUpdate(SparkleUpdater updater, AppCastItem item)
    {
        Updater = updater;
        Item = item;
    }

    internal SparkleUpdater Updater { get; }
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
