using Microsoft.UI.Xaml;

namespace ResticPal.UI;

public partial class App : Application
{
    private const string InstanceMutexName = @"Local\ResticPal.Settings";

    private Window? _window;
    private Mutex? _instanceMutex;

    public App()
    {
        InitializeComponent();
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        _instanceMutex = new Mutex(initiallyOwned: true, InstanceMutexName, out bool createdNew);
        if (!createdNew)
        {
            _instanceMutex.Dispose();
            _instanceMutex = null;
            Exit();
            return;
        }

        string[] arguments = Environment.GetCommandLineArgs().Skip(1).ToArray();
        bool showOnboarding = arguments.Contains("--setup", StringComparer.OrdinalIgnoreCase);
        bool showUpdates = arguments.Contains("--updates", StringComparer.OrdinalIgnoreCase);
        _window = new MainWindow(showOnboarding, showUpdates);
        _window.Closed += (_, _) => ReleaseInstanceMutex();
        _window.Activate();
    }

    private void ReleaseInstanceMutex()
    {
        _instanceMutex?.ReleaseMutex();
        _instanceMutex?.Dispose();
        _instanceMutex = null;
    }
}
