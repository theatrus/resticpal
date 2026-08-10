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
        bool showOnboarding = args.Arguments
            .Split(' ', StringSplitOptions.RemoveEmptyEntries)
            .Contains("--setup", StringComparer.OrdinalIgnoreCase);
        _window = new MainWindow(showOnboarding);
        _window.Activate();
    }
}
