using System.Diagnostics.CodeAnalysis;

using Microsoft.UI.Composition;
using Microsoft.UI.Composition.SystemBackdrops;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;

using Windows.UI;

namespace ResourcesHelper.Tray.Flyout;

/// <summary>
/// Desktop acrylic with the handoff tints. DesktopAcrylicBackdrop takes no tint, and its
/// configuration always turns the surface solid while the window is inactive; owning the
/// controller and the configuration lets a pinned flyout keep the active look.
/// </summary>
[SuppressMessage(
    "Design",
    "CA1001",
    Justification = "XAML owns the lifetime: the controller is disposed in OnTargetDisconnected.")]
public sealed partial class AcrylicBackdrop : SystemBackdrop
{
    private static readonly Color LightTint = Color.FromArgb(255, 243, 243, 243);
    private static readonly Color DarkTint = Color.FromArgb(255, 36, 36, 36);

    private readonly SystemBackdropConfiguration _configuration = new() { IsInputActive = true };
    private DesktopAcrylicController? _controller;
    private ElementTheme _theme = ElementTheme.Light;
    private bool _windowActive = true;
    private bool _keepActiveLook;
    // Picked by eye in M0.2 over the handoff's CSS .86; revisit with the M4.1 tokens.
    private float _tintOpacity = 0.2f;
    private float _luminosityOpacity = 0.2f;

    /// <summary>
    /// Keeps acrylic while the window is inactive instead of the solid fallback.
    /// </summary>
    public bool KeepActiveLook
    {
        get => _keepActiveLook;
        set
        {
            _keepActiveLook = value;
            ApplyActivation();
        }
    }

    public float TintOpacity
    {
        get => _tintOpacity;
        set
        {
            _tintOpacity = value;
            ApplyTint();
        }
    }

    public float LuminosityOpacity
    {
        get => _luminosityOpacity;
        set
        {
            _luminosityOpacity = value;
            ApplyTint();
        }
    }

    public void SetTheme(ElementTheme theme)
    {
        _theme = theme == ElementTheme.Dark ? ElementTheme.Dark : ElementTheme.Light;
        _configuration.Theme = _theme == ElementTheme.Dark
            ? SystemBackdropTheme.Dark
            : SystemBackdropTheme.Light;
        ApplyTint();
    }

    public void SetWindowActive(bool active)
    {
        _windowActive = active;
        ApplyActivation();
    }

    protected override void OnTargetConnected(
        ICompositionSupportsSystemBackdrop connectedTarget,
        XamlRoot xamlRoot)
    {
        base.OnTargetConnected(connectedTarget, xamlRoot);
        _controller = new DesktopAcrylicController { Kind = DesktopAcrylicKind.Base };
        ApplyTint();
        _ = _controller.AddSystemBackdropTarget(connectedTarget);
        _controller.SetSystemBackdropConfiguration(_configuration);
    }

    protected override void OnTargetDisconnected(
        ICompositionSupportsSystemBackdrop disconnectedTarget)
    {
        base.OnTargetDisconnected(disconnectedTarget);
        if (_controller is null)
        {
            return;
        }

        _ = _controller.RemoveSystemBackdropTarget(disconnectedTarget);
        _controller.Dispose();
        _controller = null;
    }

    private void ApplyActivation() =>
        _configuration.IsInputActive = _windowActive || _keepActiveLook;

    private void ApplyTint()
    {
        if (_controller is null)
        {
            return;
        }

        var tint = _theme == ElementTheme.Dark ? DarkTint : LightTint;
        _controller.TintColor = tint;
        _controller.FallbackColor = tint;
        _controller.TintOpacity = _tintOpacity;
        _controller.LuminosityOpacity = _luminosityOpacity;
    }
}
