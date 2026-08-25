using System.ComponentModel;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace ResticPal.UI.Services;

internal static class ServicePipeIdentity
{
    internal const uint ServiceRunning = 4;

    private const uint ScManagerConnect = 0x0001;
    private const uint ServiceQueryStatus = 0x0004;
    private const int ScStatusProcessInfo = 0;

    internal static void Verify(NamedPipeClientStream pipe)
    {
        ArgumentNullException.ThrowIfNull(pipe);
        if (!pipe.IsConnected || pipe.SafePipeHandle.IsInvalid || pipe.SafePipeHandle.IsClosed)
        {
            throw new InvalidDataException(
                "The resticpal service connection closed before its identity could be verified.");
        }

        if (!GetNamedPipeServerProcessId(pipe.SafePipeHandle, out uint pipeProcessId))
        {
            ThrowVerificationFailure("identify the named-pipe server process");
        }

        nint manager = OpenSCManager(null, null, ScManagerConnect);
        if (manager == 0)
        {
            ThrowVerificationFailure("connect to the Windows Service Control Manager");
        }

        try
        {
            nint service = OpenService(manager, "ResticPal", ServiceQueryStatus);
            if (service == 0)
            {
                ThrowVerificationFailure("open the registered ResticPal Windows service");
            }

            try
            {
                if (!QueryServiceStatusEx(
                        service,
                        ScStatusProcessInfo,
                        out ServiceStatusProcess status,
                        checked((uint)Marshal.SizeOf<ServiceStatusProcess>()),
                        out _))
                {
                    ThrowVerificationFailure("query the registered ResticPal service process");
                }

                if (!MatchesRunningService(
                        pipeProcessId,
                        status.CurrentState,
                        status.ProcessId))
                {
                    throw new InvalidDataException(
                        "Refusing to send data to an untrusted resticpal named-pipe server: " +
                        $"pipe process {pipeProcessId} does not match running ResticPal " +
                        $"service process {status.ProcessId} (service state {status.CurrentState}).");
                }
            }
            finally
            {
                CloseServiceHandle(service);
            }
        }
        finally
        {
            CloseServiceHandle(manager);
        }
    }

    internal static bool MatchesRunningService(
        uint pipeProcessId,
        uint serviceState,
        uint serviceProcessId)
    {
        return pipeProcessId != 0
            && serviceProcessId != 0
            && serviceState == ServiceRunning
            && pipeProcessId == serviceProcessId;
    }

    private static void ThrowVerificationFailure(string operation)
    {
        int error = Marshal.GetLastWin32Error();
        string reason = new Win32Exception(error).Message;
        throw new InvalidDataException(
            $"Could not {operation}; the resticpal connection was rejected " +
            $"before sending any data (Windows error {error}: {reason}).");
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ServiceStatusProcess
    {
        internal uint ServiceType;
        internal uint CurrentState;
        internal uint ControlsAccepted;
        internal uint Win32ExitCode;
        internal uint ServiceSpecificExitCode;
        internal uint CheckPoint;
        internal uint WaitHint;
        internal uint ProcessId;
        internal uint ServiceFlags;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetNamedPipeServerProcessId(
        SafePipeHandle pipe,
        out uint serverProcessId);

    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern nint OpenSCManager(
        string? machineName,
        string? databaseName,
        uint desiredAccess);

    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern nint OpenService(
        nint manager,
        string serviceName,
        uint desiredAccess);

    [DllImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool QueryServiceStatusEx(
        nint service,
        int informationLevel,
        out ServiceStatusProcess status,
        uint bufferSize,
        out uint bytesNeeded);

    [DllImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CloseServiceHandle(nint serviceHandle);
}
