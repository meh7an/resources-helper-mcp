using System.Diagnostics.CodeAnalysis;

using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.Windows.AppLifecycle;

using ResourcesHelper.Tray.Flyout;
using ResourcesHelper.Tray.Spike;

namespace ResourcesHelper.Tray;

[SuppressMessage(
    "Design",
    "CA1001",
    Justification = "The app lives as long as the process; Quit disposes the tray icon.")]
public sealed partial class App : Application
{
    private DispatcherQueue? _dispatcher;
    private FlyoutWindow? _flyout;
    private SpikeTray? _tray;

    public App()
    {
        InitializeComponent();

        // A tray app has no window most of the time; it ends only on an explicit quit.
        DispatcherShutdownMode = DispatcherShutdownMode.OnExplicitShutdown;
    }

    /// <summary>
    /// Removes the tray icon and ends the app; the service keeps running.
    /// </summary>
    public void Quit()
    {
        _tray?.Dispose();
        SpikeToasts.Unregister();
        Exit();
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        _dispatcher = DispatcherQueue.GetForCurrentThread();
        AppInstance.GetCurrent().Activated += OnRedirectedActivation;
        // M0.2 spike trace; M4.2 replaces it with real logging.
        UnhandledException += (_, e) => SpikeLog.Write($"unhandled: {e.Exception}");
        SpikeLog.Write("launched");

        // M0.3 spike tray and toasts; M4.2 and M5.6 replace them.
        SpikeToasts.Register(_dispatcher, OnToastAction);
        _tray = new SpikeTray(ToggleFlyout, OpenFlyout, Quit);

        // A toast clicked while the app was not running starts it with that activation.
        if (SpikeToasts.ActionOf(AppInstance.GetCurrent().GetActivatedEventArgs()) is { } action)
        {
            SpikeLog.Write($"launched by toast: action '{action}'");
            OnToastAction(action);
        }
    }

    // A second launch means the user wants to see the flyout. The event arrives on a background
    // thread.
    private void OnRedirectedActivation(object? sender, AppActivationArguments args)
    {
        SpikeLog.Write($"redirected activation: {args.Kind}");
        var action = SpikeToasts.ActionOf(args);
        _ = _dispatcher?.TryEnqueue(() =>
        {
            if (action is null)
            {
                OpenFlyout();
            }
            else
            {
                OnToastAction(action);
            }
        });
    }

    private void OnToastAction(string action)
    {
        switch (action)
        {
            case SpikeToasts.OpenLogs:
                SpikeLog.Open();
                break;
            case SpikeToasts.StartService:
                SpikeLog.Write("toast: start service (no action in the spike)");
                break;
            default:
                OpenFlyout();
                break;
        }
    }

    private void ToggleFlyout() => WithFlyout(flyout => flyout.Toggle());

    private void OpenFlyout() => WithFlyout(flyout => flyout.Open());

    private void WithFlyout(Action<FlyoutWindow> action)
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

            action(_flyout);
        }
        catch (Exception e)
        {
            SpikeLog.Write($"flyout failed: {e}");
            throw;
        }
    }
}
