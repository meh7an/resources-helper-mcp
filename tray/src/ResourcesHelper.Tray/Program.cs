using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.Windows.AppLifecycle;

using ResourcesHelper.Tray.Core;
using ResourcesHelper.Tray.Interop;

namespace ResourcesHelper.Tray;

public static class Program
{
    [STAThread]
    private static void Main()
    {
        WinRT.ComWrappersSupport.InitializeComWrappers();

        if (RedirectToRunningInstance())
        {
            return;
        }

        Application.Start(p =>
        {
            var context = new DispatcherQueueSynchronizationContext(
                DispatcherQueue.GetForCurrentThread());
            SynchronizationContext.SetSynchronizationContext(context);
            _ = new App();
        });
    }

    /// <summary>
    /// Hands this launch to the instance that owns the key, if another one does.
    /// </summary>
    /// <returns>True when this process should exit.</returns>
    private static bool RedirectToRunningInstance()
    {
        var owner = AppInstance.FindOrRegisterForKey(TrayIdentity.InstanceKey);
        if (owner.IsCurrent)
        {
            return false;
        }

        // This launch holds the foreground right; without passing it on, the running instance
        // can't bring its flyout to the front.
        _ = NativeMethods.AllowSetForegroundWindow(owner.ProcessId);
        var args = AppInstance.GetCurrent().GetActivatedEventArgs();
        // Awaiting the redirect on the STA main thread can deadlock; a pool thread does the
        // call while this blocking wait keeps pumping COM.
        Task.Run(() => owner.RedirectActivationToAsync(args).AsTask()).Wait();
        return true;
    }
}
