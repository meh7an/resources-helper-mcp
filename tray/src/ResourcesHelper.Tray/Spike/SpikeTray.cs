using Microsoft.UI.Dispatching;

using ResourcesHelper.Tray.Core.TrayIcon;
using ResourcesHelper.Tray.TrayIcon;

namespace ResourcesHelper.Tray.Spike;

/// <summary>
/// M0.3 spike only: the tray icon with the handoff menu (section 8) plus items to switch the
/// icon state and send a test toast. M4.2 replaces it with the tray driven by live data.
/// </summary>
internal sealed class SpikeTray : IDisposable
{
    private const uint OpenId = 1;
    private const uint ServiceId = 2;
    private const uint LaunchAtLogonId = 3;
    private const uint OpenLogsId = 4;
    private const uint ToastId = 5;
    private const uint QuitId = 6;
    private const uint StateBaseId = 100;

    private readonly NotifyIcon _icon;
    private readonly DispatcherQueue _dispatcher = DispatcherQueue.GetForCurrentThread();
    private readonly Action _toggleFlyout;
    private readonly Action _openFlyout;
    private readonly Action _quit;
    private TrayIconState _state = TrayIconState.Idle;
    private bool _launchAtLogon = true;

    public SpikeTray(Action toggleFlyout, Action openFlyout, Action quit)
    {
        _toggleFlyout = toggleFlyout;
        _openFlyout = openFlyout;
        _quit = quit;

        _icon = new NotifyIcon(_state, TrayIconAsset.Tooltip(Summary(_state)));
        _icon.Diagnostic += message => SpikeLog.Write($"tray: {message}");
        _icon.Selected += OnSelected;
        _icon.ContextMenuRequested += OnContextMenuRequested;
        _icon.ThemeChanged += UseMenuTheme;
        UseMenuTheme(_icon.Theme);
        _icon.Show();
    }

    public void Dispose() => _icon.Dispose();

    private static string Summary(TrayIconState state) => state switch
    {
        TrayIconState.Idle => "Idle",
        TrayIconState.Busy => "1 running",
        TrayIconState.Queued => "1 running · 1 waiting",
        TrayIconState.Down => "Service stopped",
        _ => throw new ArgumentOutOfRangeException(nameof(state)),
    };

    private static void UseMenuTheme(TaskbarTheme theme) =>
        SpikeLog.Write($"tray: menu theme {theme}, applied {NativeMenu.UseTheme(theme)}");

    // The click arrives inside the icon's window procedure; the flyout work runs after it.
    private void OnSelected(TrayCallback callback) =>
        _ = _dispatcher.TryEnqueue(() => _toggleFlyout());

    private void OnContextMenuRequested(TrayCallback callback)
    {
        var id = NativeMenu.Show(_icon.WindowHandle, BuildMenu(), callback.X, callback.Y);
        SpikeLog.Write($"tray: menu returned {id}");
        if (id != 0)
        {
            _ = _dispatcher.TryEnqueue(() => Run(id));
        }
    }

    private List<NativeMenuItem> BuildMenu() =>
    [
        new(Summary(_state)) { Enabled = false },
        NativeMenuItem.Separator,
        new("Open Resources Helper", OpenId) { IsDefault = true },
        new(_state == TrayIconState.Down ? "Start service" : "Stop service…", ServiceId),
        new("Launch at logon", LaunchAtLogonId) { Checked = _launchAtLogon },
        new("Open logs", OpenLogsId),
        NativeMenuItem.Separator,
        new("Spike: icon state")
        {
            Children =
            [
                .. Enum.GetValues<TrayIconState>().Select(state =>
                    new NativeMenuItem(state.ToString(), StateBaseId + (uint)state)
                    {
                        Checked = state == _state,
                    }),
            ],
        },
        new("Spike: send test toast", ToastId),
        NativeMenuItem.Separator,
        new("Quit", QuitId),
    ];

    private void Run(uint id)
    {
        switch (id)
        {
            case OpenId:
                _openFlyout();
                break;
            case ServiceId:
                SpikeLog.Write("tray: service item chosen (no action in the spike)");
                break;
            case LaunchAtLogonId:
                _launchAtLogon = !_launchAtLogon;
                break;
            case OpenLogsId:
                SpikeLog.Open();
                break;
            case ToastId:
                SpikeToasts.Show();
                break;
            case QuitId:
                _quit();
                break;
            case >= StateBaseId:
                SetState((TrayIconState)(id - StateBaseId));
                break;
        }
    }

    private void SetState(TrayIconState state)
    {
        _state = state;
        _icon.SetState(state);
        _icon.SetTooltip(TrayIconAsset.Tooltip(Summary(state)));
    }
}
