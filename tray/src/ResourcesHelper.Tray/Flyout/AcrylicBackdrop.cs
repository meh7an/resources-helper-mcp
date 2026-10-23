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
    // Picked by eye over the handoff's CSS .86 (dark in M0.2, light in M0.3); revisit with the
    // M4.1 tokens.
    private float _lightTintOpacity = 0.5f;
    private float _darkTintOpacity = 0.2f;
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

    /// <summary>
    /// The tint opacity of the current theme; light needs more than dark.
    /// </summary>
    public float TintOpacity
    {
        get => _theme == ElementTheme.Dark ? _darkTintOpacity : _lightTintOpacity;
        set
        {
            if (_theme == ElementTheme.Dark)
            {
                _darkTintOpacity = value;
            }
            else
            {
                _lightTintOpacity = value;
            }

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

    // The base implementation throws E_INVALIDARG for our target on every system theme change,
    // which ends the app. This backdrop owns its configuration and takes the theme from the
    // content's ActualThemeChanged instead.
    protected override void OnDefaultSystemBackdropConfigurationChanged(
        ICompositionSupportsSystemBackdrop target,
        XamlRoot xamlRoot)
    {
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
        _controller.TintOpacity = TintOpacity;
        _controller.LuminosityOpacity = _luminosityOpacity;
    }
}
