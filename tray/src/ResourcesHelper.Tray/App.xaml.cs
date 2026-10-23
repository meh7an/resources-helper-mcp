using Microsoft.UI.Xaml;
using Microsoft.Windows.AppLifecycle;

namespace ResourcesHelper.Tray;

public sealed partial class App : Application
{
    public App()
    {
        InitializeComponent();

        // A tray app has no window most of the time; it ends only on an explicit quit.
        DispatcherShutdownMode = DispatcherShutdownMode.OnExplicitShutdown;
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args) =>
        AppInstance.GetCurrent().Activated += OnRedirectedActivation;

    private void OnRedirectedActivation(object? sender, AppActivationArguments args)
    {
        // TODO: show the flyout once it exists; a second launch means the user wants to see it.
        // This event arrives on a background thread, so marshal to the UI dispatcher first.
    }
}
