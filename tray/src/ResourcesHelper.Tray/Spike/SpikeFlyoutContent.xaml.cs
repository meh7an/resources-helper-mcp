using System.Globalization;

using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;

using ResourcesHelper.Tray.Flyout;

using Windows.Foundation;

namespace ResourcesHelper.Tray.Spike;

/// <summary>
/// M0.2 spike only: drives the flyout window's behaviors and shows what it measures.
/// </summary>
public sealed partial class SpikeFlyoutContent : UserControl
{
    public const double SpikeHeight = 520;

    private const string LockOpenGlyph = "";
    private const string LockClosedGlyph = "";
    private const string PinGlyph = "";
    private const string PinnedGlyph = "";

    private readonly FlyoutWindow _window;
    private readonly DispatcherTimer _refresh = new() { Interval = TimeSpan.FromMilliseconds(500) };

    public SpikeFlyoutContent(FlyoutWindow window)
    {
        _window = window;
        InitializeComponent();

        TintSlider.Value = _window.Backdrop.TintOpacity;
        LuminositySlider.Value = _window.Backdrop.LuminosityOpacity;
        UpdateSliderLabels();

        _window.Diagnostic += SpikeLog.Write;
        _window.ModesChanged += SyncToggles;
        _window.Opened += () => StayOpenButton.Focus(FocusState.Programmatic);
        _refresh.Tick += (_, _) => DiagnosticsText.Text = _window.Describe();
        _refresh.Start();
    }

    private void OnStayOpenClick(object sender, RoutedEventArgs e) =>
        _window.SetStayOpen(StayOpenButton.IsChecked == true);

    private void OnPinClick(object sender, RoutedEventArgs e) =>
        _window.SetPinned(PinButton.IsChecked == true);

    private void OnHeaderSizeChanged(object sender, SizeChangedEventArgs e) => ReportDragRegion();

    /// <summary>
    /// The header from its left edge up to the first button: glyph and summary, no controls.
    /// </summary>
    private void ReportDragRegion()
    {
        var firstButton = PinButton.Visibility == Visibility.Visible ? PinButton : StayOpenButton;
        var header = Header.TransformToVisual(this).TransformPoint(default);
        var buttonLeft = firstButton.TransformToVisual(this).TransformPoint(default).X;
        _window.SetDragRegion(
            new Rect(header.X, header.Y, buttonLeft - header.X, Header.ActualHeight));
    }

    private void OnMenuOpened(object? sender, object e) => _window.SetMenuOpen(true);

    private void OnMenuClosed(object? sender, object e) => _window.SetMenuOpen(false);

    private void OnQuitClick(object sender, RoutedEventArgs e) => ((App)Application.Current).Quit();

    private void OnKeepActiveToggled(object sender, RoutedEventArgs e)
    {
        _window.Backdrop.KeepActiveLook = KeepActiveSwitch.IsOn;
        SpikeLog.Write($"keep acrylic when inactive: {KeepActiveSwitch.IsOn}");
    }

    private void OnTintChanged(object sender, RangeBaseValueChangedEventArgs e)
    {
        _window.Backdrop.TintOpacity = (float)e.NewValue;
        UpdateSliderLabels();
    }

    private void OnLuminosityChanged(object sender, RangeBaseValueChangedEventArgs e)
    {
        _window.Backdrop.LuminosityOpacity = (float)e.NewValue;
        UpdateSliderLabels();
    }

    private void UpdateSliderLabels()
    {
        if (TintLabel is null || LuminosityLabel is null)
        {
            return;
        }

        TintLabel.Text = string.Create(
            CultureInfo.InvariantCulture, $"Tint opacity {_window.Backdrop.TintOpacity:0.00}");
        LuminosityLabel.Text = string.Create(
            CultureInfo.InvariantCulture,
            $"Luminosity opacity {_window.Backdrop.LuminosityOpacity:0.00}");
    }

    private void SyncToggles()
    {
        var behavior = _window.Behavior;
        StayOpenButton.IsChecked = behavior.StayOpen;
        StayOpenIcon.Glyph = behavior.StayOpen ? LockClosedGlyph : LockOpenGlyph;
        PinButton.IsChecked = behavior.Pinned;
        PinIcon.Glyph = behavior.Pinned ? PinnedGlyph : PinGlyph;
        PinButton.Visibility = behavior.StayOpen ? Visibility.Visible : Visibility.Collapsed;
        KeepActiveSwitch.IsOn = _window.Backdrop.KeepActiveLook;
        // Pin appearing moves the button cluster's left edge; measure after layout.
        Header.UpdateLayout();
        ReportDragRegion();
    }
}
