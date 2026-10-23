using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.Windows.AppLifecycle;

using ResourcesHelper.Tray.Flyout;
using ResourcesHelper.Tray.Spike;

namespace ResourcesHelper.Tray;

public sealed partial class App : Application
{
    private DispatcherQueue? _dispatcher;
    private FlyoutWindow? _flyout;

    public App()
    {
        InitializeComponent();

        // A tray app has no window most of the time; it ends only on an explicit quit.
        DispatcherShutdownMode = DispatcherShutdownMode.OnExplicitShutdown;
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        _dispatcher = DispatcherQueue.GetForCurrentThread();
        AppInstance.GetCurrent().Activated += OnRedirectedActivation;
        // M0.2 spike trace; M4.2 replaces it with real logging.
        UnhandledException += (_, e) => SpikeLog.Write($"unhandled: {e.Exception}");
        SpikeLog.Write("launched");
    }

    // A second launch means the user wants to see the flyout. Until the tray icon exists
    // (M0.3) it is also the toggle. The event arrives on a background thread.
    private void OnRedirectedActivation(object? sender, AppActivationArguments args)
    {
        SpikeLog.Write($"redirected activation: {args.Kind}");
        _ = _dispatcher?.TryEnqueue(ToggleFlyout);
    }

    private void ToggleFlyout()
    {
        try
        {
            if (_flyout is null)
            {
                _flyout = new FlyoutWindow();
                // M0.2 spike content; M4.2 replaces it with the production shell.
                _flyout.SetContent(
                    new SpikeFlyoutContent(_flyout), SpikeFlyoutContent.SpikeHeight);
            }

            _flyout.Toggle();
        }
        catch (Exception e)
        {
            SpikeLog.Write($"toggle failed: {e}");
            throw;
        }
    }
}
