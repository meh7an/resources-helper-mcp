using System.Globalization;
using System.Numerics;
using System.Runtime.InteropServices;

using Microsoft.UI;
using Microsoft.UI.Composition;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Input;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Hosting;
using Microsoft.UI.Xaml.Input;

using ResourcesHelper.Tray.Core.Flyout;
using ResourcesHelper.Tray.Interop;

using Windows.Graphics;
using Windows.UI.ViewManagement;

using VirtualKey = Windows.System.VirtualKey;

namespace ResourcesHelper.Tray.Flyout;

/// <summary>
/// The borderless acrylic popup next to the tray: no taskbar button, no Alt+Tab entry, light
/// dismiss, Stay open, Pin and pinned dragging. Window plumbing only; the content is supplied.
/// </summary>
public sealed partial class FlyoutWindow : Window
{
    // A second launch needs several hundred ms to reach the running instance; M0.3 sets the
    // value for a real tray click.
    private static readonly TimeSpan ToggleDebounce = TimeSpan.FromMilliseconds(500);
    private static readonly TimeSpan OpenDuration = TimeSpan.FromMilliseconds(167);
    private static readonly TimeSpan CloseDuration = TimeSpan.FromMilliseconds(100);
    private const float OpenOffset = 12;

    private readonly FlyoutBehavior _behavior = new(ToggleDebounce);
    private readonly AcrylicBackdrop _backdrop = new();
    private readonly OverlappedPresenter _presenter = OverlappedPresenter.Create();
    private readonly UISettings _uiSettings = new();
    private readonly DispatcherQueue _dispatcher;
    private readonly nint _hwnd;
    private FrameworkElement? _root;
    private double _contentHeight = FlyoutPlacement.MaxHeight;
    private bool _framed;
    private bool _active;
    private long _deactivatedAtMs;
    private long _lastToggleGapMs = -1;
    private int _generation;
    private bool _wasPinned;
    private Windows.Foundation.Rect _dragRegion;

    public FlyoutWindow()
    {
        _dispatcher = DispatcherQueue.GetForCurrentThread();
        _hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);

        _presenter.SetBorderAndTitleBar(hasBorder: true, hasTitleBar: false);
        _presenter.IsResizable = false;
        _presenter.IsMaximizable = false;
        _presenter.IsMinimizable = false;
        AppWindow.SetPresenter(_presenter);
        AppWindow.IsShownInSwitchers = false;
        AppWindow.Title = "Resources Helper";
        AppWindow.Closing += OnClosing;

        SystemBackdrop = _backdrop;
        Activated += OnActivated;
    }

    /// <summary>
    /// Raised after the open animation starts, so the content can move focus to its first control.
    /// </summary>
    public event Action? Opened;

    /// <summary>
    /// Raised when Stay open or Pin changes, including the reset on a tray close.
    /// </summary>
    public event Action? ModesChanged;

    public event Action<string>? Diagnostic;

    public FlyoutBehavior Behavior => _behavior;

    public AcrylicBackdrop Backdrop => _backdrop;

    private static long Now => Environment.TickCount64;

    private bool AnimationsEnabled => _uiSettings.AnimationsEnabled;

    public void SetContent(FrameworkElement root, double contentHeight)
    {
        _root = root;
        _contentHeight = contentHeight;
        Content = root;
        ElementCompositionPreview.SetIsTranslationEnabled(root, true);
        root.AddHandler(UIElement.KeyDownEvent, new KeyEventHandler(OnKeyDown), true);
        root.ActualThemeChanged += (sender, _) => _backdrop.SetTheme(sender.ActualTheme);
        root.Loaded += (_, _) => _backdrop.SetTheme(root.ActualTheme);
        root.Loaded += (_, _) => root.XamlRoot.Changed += OnXamlRootChanged;
    }

    /// <summary>
    /// A tray click or a second launch.
    /// </summary>
    public void Toggle()
    {
        var now = Now;
        _lastToggleGapMs = _active ? -1 : now - _deactivatedAtMs;
        var result = _behavior.Toggle(now);
        Report($"toggle -> {result} (gap since deactivation {_lastToggleGapMs} ms)");
        switch (result)
        {
            case ToggleResult.Show:
                Show();
                break;
            case ToggleResult.Hide:
                ApplyModes();
                Hide();
                break;
            case ToggleResult.Ignore:
                break;
        }
    }

    public void SetStayOpen(bool on)
    {
        _behavior.SetStayOpen(on);
        ApplyModes();
    }

    public void SetPinned(bool on)
    {
        _behavior.SetPinned(on);
        ApplyModes();
    }

    public void SetMenuOpen(bool open)
    {
        _behavior.MenuOpen = open;
        Report($"menu {(open ? "opened" : "closed")}, foreground ours {ForegroundIsOurs()}");

        // A click on another app closes the menu; if the flyout itself was not reactivated by
        // that click, it has lost activation while the dismiss was suppressed.
        if (!open && !ForegroundIsOurs() && _behavior.Deactivated(Now))
        {
            _deactivatedAtMs = Now;
            Hide();
        }
    }

    /// <summary>
    /// The empty header space, in effective pixels relative to the content root. While pinned
    /// it becomes a caption region, so Windows runs the drag natively. Sending
    /// WM_NCLBUTTONDOWN from a XAML pointer press does not work: XAML input arrives through a
    /// separate input window, so the move loop never sees the button go up and keeps
    /// following the cursor after the click.
    /// </summary>
    public void SetDragRegion(Windows.Foundation.Rect region)
    {
        _dragRegion = region;
        ApplyDragRegion();
    }

    /// <summary>
    /// The live window facts the spike checklist reads.
    /// </summary>
    public string Describe()
    {
        var area = DisplayArea.GetFromWindowId(AppWindow.Id, DisplayAreaFallback.Nearest);
        var work = ToPixelRect(area.WorkArea);
        var visible = VisibleBounds();
        var scale = _root?.XamlRoot?.RasterizationScale ?? 0;
        var taskbar = FlyoutPlacement.IsTaskbarAtTop(work, ToPixelRect(area.OuterBounds))
            ? "top"
            : "bottom";
        var dpi = NativeMethods.GetDpiForWindow(_hwnd);
        return string.Create(
            CultureInfo.InvariantCulture,
            $"""
            scale {scale:0.##}  dpi {dpi}  primary {area.IsPrimary}
            visible {visible.X},{visible.Y} {visible.Width}x{visible.Height}
            window {DescribeBounds()}
            work {work.X},{work.Y} {work.Width}x{work.Height}  taskbar {taskbar}
            margin right {work.Right - visible.Right}  bottom {work.Bottom - visible.Bottom}
            margin top {visible.Y - work.Y}
            active {_active}  stay {_behavior.StayOpen}  pin {_behavior.Pinned}
            topmost {_presenter.IsAlwaysOnTop}  menu {_behavior.MenuOpen}
            animations {AnimationsEnabled}  toggle gap {_lastToggleGapMs} ms
            """);
    }

    private static PixelRect ToPixelRect(RectInt32 rect) =>
        new(rect.X, rect.Y, rect.Width, rect.Height);

    private static double ScaleOf(DisplayArea area)
    {
        var monitor = Win32Interop.GetMonitorFromDisplayId(area.DisplayId);
        var hr = NativeMethods.GetDpiForMonitor(
            monitor, NativeMethods.MdtEffectiveDpi, out var dpi, out _);
        return hr == 0 ? dpi / 96.0 : 1.0;
    }

    private static bool ForegroundIsOurs()
    {
        var foreground = NativeMethods.GetForegroundWindow();
        if (foreground == 0)
        {
            return false;
        }

        _ = NativeMethods.GetWindowThreadProcessId(foreground, out var processId);
        return processId == (uint)Environment.ProcessId;
    }

    private void ApplyDwmFrame()
    {
        var corner = NativeMethods.DwmSetWindowAttribute(
            _hwnd,
            NativeMethods.DwmwaWindowCornerPreference,
            NativeMethods.DwmwcpRound,
            sizeof(int));
        // The 1 px edge is drawn by the content in the handoff colours, so DWM draws none.
        var border = NativeMethods.DwmSetWindowAttribute(
            _hwnd, NativeMethods.DwmwaBorderColor, NativeMethods.DwmwaColorNone, sizeof(int));
        Report($"dwm corner hr 0x{corner:X8}, border hr 0x{border:X8}");
    }

    private void Show()
    {
        var generation = ++_generation;
        if (!_framed)
        {
            ApplyDwmFrame();
            _framed = true;
        }

        PlaceAtTrayCorner();
        _presenter.IsAlwaysOnTop = false;
        PrepareOpen();
        AppWindow.Show();
        Activate();
        if (!NativeMethods.SetForegroundWindow(_hwnd))
        {
            Report("SetForegroundWindow refused");
        }

        PlayOpen(generation);
        Opened?.Invoke();
    }

    private void Hide()
    {
        var generation = ++_generation;
        if (_root is null)
        {
            AppWindow.Hide();
            return;
        }

        var visual = ElementCompositionPreview.GetElementVisual(_root);
        var compositor = visual.Compositor;
        var fade = compositor.CreateScalarKeyFrameAnimation();
        fade.InsertKeyFrame(1, 0, compositor.CreateLinearEasingFunction());
        fade.Duration = CloseDuration;

        var batch = compositor.CreateScopedBatch(CompositionBatchTypes.Animation);
        visual.StartAnimation("Opacity", fade);
        batch.End();
        batch.Completed += (_, _) => _dispatcher.TryEnqueue(() =>
        {
            // A toggle during the fade reopened the flyout; keep it.
            if (generation == _generation)
            {
                AppWindow.Hide();
            }
        });
    }

    private void PlaceAtTrayCorner()
    {
        var area = DisplayArea.Primary;
        var scale = ScaleOf(area);
        var target = FlyoutPlacement.TrayCorner(
            ToPixelRect(area.WorkArea), ToPixelRect(area.OuterBounds), scale, _contentHeight);
        AppWindow.MoveAndResize(new RectInt32(target.X, target.Y, target.Width, target.Height));

        // The window rectangle can include an invisible frame; grow it so the visible edge lands
        // exactly on the target.
        var window = WindowBounds();
        var visible = VisibleBounds();
        var left = visible.X - window.X;
        var top = visible.Y - window.Y;
        var right = window.Right - visible.Right;
        var bottom = window.Bottom - visible.Bottom;
        if (left != 0 || top != 0 || right != 0 || bottom != 0)
        {
            AppWindow.MoveAndResize(new RectInt32(
                target.X - left,
                target.Y - top,
                target.Width + left + right,
                target.Height + top + bottom));
        }

        Report(string.Create(
            CultureInfo.InvariantCulture,
            $"placed at scale {scale:0.##}: {target}, frame insets {left},{top},{right},{bottom}"));
    }

    private void PrepareOpen()
    {
        if (_root is null)
        {
            return;
        }

        var visual = ElementCompositionPreview.GetElementVisual(_root);
        visual.Opacity = 0;
        var offset = AnimationsEnabled ? new Vector3(0, OpenOffset, 0) : Vector3.Zero;
        visual.Properties.InsertVector3("Translation", offset);
    }

    private void PlayOpen(int generation)
    {
        if (_root is null || generation != _generation)
        {
            return;
        }

        var visual = ElementCompositionPreview.GetElementVisual(_root);
        var compositor = visual.Compositor;

        var fade = compositor.CreateScalarKeyFrameAnimation();
        fade.InsertKeyFrame(1, 1, compositor.CreateLinearEasingFunction());
        fade.Duration = OpenDuration;
        visual.StartAnimation("Opacity", fade);

        if (AnimationsEnabled)
        {
            var decelerate = compositor.CreateCubicBezierEasingFunction(
                new Vector2(0.1f, 0.9f), new Vector2(0.2f, 1f));
            var slide = compositor.CreateVector3KeyFrameAnimation();
            slide.InsertKeyFrame(1, Vector3.Zero, decelerate);
            slide.Duration = OpenDuration;
            visual.StartAnimation("Translation", slide);
        }
    }

    private void ApplyModes()
    {
        _presenter.IsAlwaysOnTop = _behavior.Pinned;
        // A flyout kept open is a mini monitor; it keeps acrylic instead of turning solid
        // when another window is active (Mehran's M0.2 decision).
        _backdrop.KeepActiveLook = _behavior.StayOpen;
        Report($"modes: stay {_behavior.StayOpen}, pin {_behavior.Pinned}");
        ModesChanged?.Invoke();
        ApplyDragRegion();

        // Unpinning (directly or by turning Stay open off) returns a dragged flyout home.
        if (_wasPinned && !_behavior.Pinned && _behavior.IsOpen)
        {
            PlaceAtTrayCorner();
        }

        _wasPinned = _behavior.Pinned;
    }

    private void ApplyDragRegion()
    {
        var source = InputNonClientPointerSource.GetForWindowId(AppWindow.Id);
        var scale = _root?.XamlRoot?.RasterizationScale ?? 1.0;
        if (!_behavior.Pinned || _dragRegion.Width <= 0 || _dragRegion.Height <= 0)
        {
            source.ClearRegionRects(NonClientRegionKind.Caption);
            return;
        }

        var caption = new RectInt32(
            FlyoutPlacement.ToPixels(_dragRegion.X, scale),
            FlyoutPlacement.ToPixels(_dragRegion.Y, scale),
            FlyoutPlacement.ToPixels(_dragRegion.Width, scale),
            FlyoutPlacement.ToPixels(_dragRegion.Height, scale));
        source.SetRegionRects(NonClientRegionKind.Caption, [caption]);
        Report($"drag region {caption.X},{caption.Y} {caption.Width}x{caption.Height}");
    }

    private void OnActivated(object sender, WindowActivatedEventArgs args)
    {
        _active = args.WindowActivationState != WindowActivationState.Deactivated;
        _backdrop.SetWindowActive(_active);
        Report($"activation {args.WindowActivationState}");
        if (_active)
        {
            return;
        }

        _deactivatedAtMs = Now;
        if (_behavior.Deactivated(_deactivatedAtMs))
        {
            Hide();
        }
    }

    private void OnKeyDown(object sender, KeyRoutedEventArgs args)
    {
        if (args.Key == VirtualKey.Escape && _behavior.Escape())
        {
            args.Handled = true;
            Hide();
        }
    }

    private void OnClosing(AppWindow sender, AppWindowClosingEventArgs args)
    {
        // Alt+F4 must not destroy the only flyout window; treat it like a dismiss.
        args.Cancel = true;
        if (_behavior.IsOpen)
        {
            _ = _behavior.Toggle(Now);
            ApplyModes();
            Hide();
        }
    }

    private void OnXamlRootChanged(XamlRoot sender, XamlRootChangedEventArgs args)
    {
        Report($"xaml root changed: scale {sender.RasterizationScale:0.##}, {DescribeBounds()}");
        // Region rects are physical pixels; a monitor with another scale needs them redone.
        ApplyDragRegion();
    }

    private PixelRect WindowBounds() =>
        NativeMethods.GetWindowRect(_hwnd, out var rect)
            ? new PixelRect(rect.Left, rect.Top, rect.Right - rect.Left, rect.Bottom - rect.Top)
            : default;

    private PixelRect VisibleBounds()
    {
        var hr = NativeMethods.DwmGetWindowAttribute(
            _hwnd,
            NativeMethods.DwmwaExtendedFrameBounds,
            out var rect,
            Marshal.SizeOf<NativeMethods.NativeRect>());
        return hr == 0
            ? new PixelRect(rect.Left, rect.Top, rect.Right - rect.Left, rect.Bottom - rect.Top)
            : WindowBounds();
    }

    private string DescribeBounds()
    {
        var bounds = WindowBounds();
        return string.Create(
            CultureInfo.InvariantCulture,
            $"{bounds.X},{bounds.Y} {bounds.Width}x{bounds.Height}");
    }

    private void Report(string message) => Diagnostic?.Invoke(message);
}
