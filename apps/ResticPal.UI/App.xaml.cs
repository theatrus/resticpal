using Microsoft.UI.Xaml;

namespace ResticPal.UI;

public partial class App : Application
{
    private const string InstanceMutexName = @"Local\ResticPal.Settings";

    private Window? _window;
    private Mutex? _instanceMutex;

    public App()
    {
        _instanceMutex = new Mutex(initiallyOwned: true, InstanceMutexName, out bool createdNew);
        if (!createdNew)
        {
            _instanceMutex.Dispose();
            _instanceMutex = null;
            // No window or mutable state exists in a secondary process. Exit
            // synchronously before loading XAML so repeated tray/shortcut
            // activations do not leave a second elevated WinUI dispatcher alive.
            Environment.Exit(0);
            return;
        }

        InitializeComponent();
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        string[] arguments = Environment.GetCommandLineArgs().Skip(1).ToArray();
        bool showOnboarding = arguments.Contains("--setup", StringComparer.OrdinalIgnoreCase);
        bool showUpdates = arguments.Contains("--updates", StringComparer.OrdinalIgnoreCase);
        _window = new MainWindow(showOnboarding, showUpdates);
        // The Settings process owns the mutex for its entire lifetime. Ask the
        // XAML application to shut down after its only window closes so WinUI
        // can unwind the dispatcher and native resources cleanly. Abruptly
        // terminating the CLR from inside Closed can fault while Restart
        // Manager is closing Settings during an MSI upgrade or uninstall.
        _window.Closed += (_, _) => Exit();
        _window.Activate();
    }
}
