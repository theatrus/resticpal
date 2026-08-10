using Microsoft.UI.Xaml;

namespace ResticPal.UI;

public partial class App : Application
{
    private Window? _window;

    public App()
    {
        InitializeComponent();
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        string[] arguments = args.Arguments
            .Split(' ', StringSplitOptions.RemoveEmptyEntries)
            .ToArray();
        bool showOnboarding = arguments.Contains("--setup", StringComparer.OrdinalIgnoreCase);
        bool showUpdates = arguments.Contains("--updates", StringComparer.OrdinalIgnoreCase);
        _window = new MainWindow(showOnboarding, showUpdates);
        _window.Activate();
    }
}
