using System.Runtime.InteropServices;

using Microsoft.UI.Dispatching;
using Microsoft.Windows.AppLifecycle;
using Microsoft.Windows.AppNotifications;
using Microsoft.Windows.AppNotifications.Builder;

namespace ResourcesHelper.Tray.Spike;

/// <summary>
/// M0.3 spike only: proves AppNotificationManager from the unpackaged, self-contained app with
/// a two-button toast. M5.6 builds the real toasts on it.
/// </summary>
internal static class SpikeToasts
{
    public const string StartService = "start-service";
    public const string OpenLogs = "open-logs";

    private const string ActionKey = "action";

    /// <param name="dispatcher">The UI thread's queue; clicks arrive on a background thread.</param>
    /// <param name="onAction">
    /// Runs on the UI thread with the clicked button's action, or an empty string for a click on
    /// the toast body.
    /// </param>
    public static void Register(DispatcherQueue dispatcher, Action<string> onAction)
    {
        var manager = AppNotificationManager.Default;
        // Attached before Register, or a click starts a second process instead of reaching
        // this one.
        manager.NotificationInvoked += (source, args) =>
        {
            var action = ActionOf(args.Arguments);
            SpikeLog.Write($"toast invoked: action '{action}'");
            _ = dispatcher.TryEnqueue(() => onAction(action));
        };

        SpikeLog.Write($"toasts supported {AppNotificationManager.IsSupported()}");
        var icon = new Uri(Path.Combine(AppContext.BaseDirectory, "Assets", "AppIcon.ico"));
        try
        {
            manager.Register("Resources Helper", icon);
            SpikeLog.Write($"toasts registered with name and {icon}");
        }
        catch (COMException e)
        {
            // Toasts are optional; the tray must keep working without them.
            SpikeLog.Write($"toast register failed: 0x{e.HResult:X8} {e.Message}");
        }
    }

    public static void Unregister() => AppNotificationManager.Default.Unregister();

    /// <returns>
    /// The action when a toast click launched or redirected to this process, otherwise null.
    /// </returns>
    public static string? ActionOf(AppActivationArguments activation) =>
        activation.Data is AppNotificationActivatedEventArgs args ? ActionOf(args.Arguments) : null;

    public static void Show()
    {
        var notification = new AppNotificationBuilder()
            .AddArgument("toast", "spike")
            .AddText("Service stopped")
            .AddText("Spike toast: both buttons should reach the running tray app.")
            .AddButton(
                new AppNotificationButton("Start service").AddArgument(ActionKey, StartService))
            .AddButton(
                new AppNotificationButton("Open logs").AddArgument(ActionKey, OpenLogs))
            .BuildNotification();
        AppNotificationManager.Default.Show(notification);
        SpikeLog.Write($"toast shown: id {notification.Id}");
    }

    private static string ActionOf(IDictionary<string, string> arguments) =>
        arguments.TryGetValue(ActionKey, out var action) ? action : string.Empty;
}
